use serde_json::{Value, json};
use simulacra_catalog::repo::{AgentRepository, ChannelRepository, TenantRepository};
use simulacra_catalog::{
    Agent, AgentPatch, Catalog, CatalogError, Channel, ChannelId, ChannelKind, ChannelPatch,
    NewAgent, NewChannel, PageRequest, Tenant,
};
use std::slice;
use tokio::time::{Duration, sleep};

fn fresh() -> Catalog {
    Catalog::open_in_memory().unwrap()
}

async fn create_tenant(catalog: &Catalog, namespace: &str) -> Tenant {
    catalog.tenants().create(namespace, None).await.unwrap()
}

async fn create_channel(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    kind: ChannelKind,
    config: Option<&Value>,
) -> Channel {
    catalog
        .channels()
        .create(&tenant.id, NewChannel { name, kind, config })
        .await
        .unwrap()
}

async fn create_agent(
    catalog: &Catalog,
    tenant: &Tenant,
    name: &str,
    channel_ids: &[ChannelId],
) -> Agent {
    catalog
        .agents()
        .create(
            &tenant.id,
            NewAgent {
                name,
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &[],
                channel_ids,
            },
        )
        .await
        .unwrap()
}

fn channel_id_strings(channels: &[Channel]) -> Vec<String> {
    channels
        .iter()
        .map(|channel| channel.id.as_str().to_owned())
        .collect()
}

fn channel_names(channels: &[Channel]) -> Vec<String> {
    channels
        .iter()
        .map(|channel| channel.name.clone())
        .collect()
}

#[tokio::test]
async fn channel_create_populates_id_timestamps_and_defaults_missing_config_to_empty_object() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let config = json!({"room": "ops", "notify": true});

    let created = create_channel(
        &catalog,
        &tenant,
        "ops-room",
        ChannelKind::Slack,
        Some(&config),
    )
    .await;
    assert!(!created.id.as_str().is_empty());
    assert!(created.created_at <= created.updated_at);
    assert_eq!(created.name, "ops-room");
    assert_eq!(created.kind, ChannelKind::Slack);
    assert_eq!(created.config, config);

    let defaulted = create_channel(
        &catalog,
        &tenant,
        "manual-intake",
        ChannelKind::Manual,
        None,
    )
    .await;
    assert_eq!(defaulted.config, json!({}));
}

#[tokio::test]
async fn channel_create_duplicate_name_conflicts_within_tenant_only() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;

    create_channel(&catalog, &alice, "ops-room", ChannelKind::Slack, None).await;

    let err = catalog
        .channels()
        .create(
            &alice.id,
            NewChannel {
                name: "ops-room",
                kind: ChannelKind::Teams,
                config: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::Conflict(_)));

    let other_tenant = create_channel(&catalog, &bob, "ops-room", ChannelKind::Teams, None).await;
    assert_eq!(other_tenant.name, "ops-room");
    assert_eq!(other_tenant.tenant_id, bob.id);
}

#[tokio::test]
async fn channel_get_is_tenant_scoped_and_hides_cross_tenant_rows() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let channel = create_channel(&catalog, &alice, "ops-room", ChannelKind::Slack, None).await;

    let fetched = catalog
        .channels()
        .get(&alice.id, &channel.id)
        .await
        .unwrap();
    assert_eq!(fetched.id.as_str(), channel.id.as_str());

    let err = catalog
        .channels()
        .get(&bob.id, &channel.id)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn channel_list_paginates_with_stable_created_at_then_id_cursor_order() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;

    let mut inserted_ids = Vec::new();
    let mut inserted_names = Vec::new();
    // Sleep between creates so RFC3339 timestamps are strictly increasing.
    for name in ["alpha", "bravo", "charlie", "delta", "echo"] {
        let channel = create_channel(&catalog, &tenant, name, ChannelKind::Manual, None).await;
        inserted_ids.push(channel.id.as_str().to_owned());
        inserted_names.push(channel.name);
        sleep(Duration::from_millis(2)).await;
    }

    let first = catalog
        .channels()
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: None,
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(channel_names(&first.items), inserted_names[..2].to_vec());
    assert_eq!(channel_id_strings(&first.items), inserted_ids[..2].to_vec());
    assert!(first.has_next_page);
    assert!(first.end_cursor.is_some());

    let second = catalog
        .channels()
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: first.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(channel_names(&second.items), inserted_names[2..4].to_vec());
    assert_eq!(
        channel_id_strings(&second.items),
        inserted_ids[2..4].to_vec()
    );
    assert!(second.has_next_page);
    assert!(second.end_cursor.is_some());

    let third = catalog
        .channels()
        .list(
            &tenant.id,
            PageRequest {
                first: Some(2),
                after: second.end_cursor.clone(),
                last: None,
                before: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(channel_names(&third.items), inserted_names[4..].to_vec());
    assert_eq!(channel_id_strings(&third.items), inserted_ids[4..].to_vec());
    assert!(!third.has_next_page);
}

#[tokio::test]
async fn channel_list_name_contains_filters_with_limit_and_stays_tenant_scoped() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;

    create_channel(&catalog, &alice, "alpha", ChannelKind::Manual, None).await;
    create_channel(&catalog, &alice, "bravo", ChannelKind::Manual, None).await;
    create_channel(&catalog, &alice, "ops-alerts", ChannelKind::Slack, None).await;
    create_channel(&catalog, &alice, "ops-pages", ChannelKind::Teams, None).await;
    create_channel(&catalog, &bob, "ops-foreign", ChannelKind::Email, None).await;

    let page = catalog
        .channels()
        .list(
            &alice.id,
            PageRequest {
                first: Some(2),
                after: None,
                last: None,
                before: None,
            },
            Some("ops"),
        )
        .await
        .unwrap();

    assert_eq!(channel_names(&page.items), vec!["ops-alerts", "ops-pages"]);
    assert!(
        page.items
            .iter()
            .all(|channel| channel.tenant_id == alice.id)
    );
    assert!(!page.has_next_page);
}

#[tokio::test]
async fn channel_update_round_trips_name_kind_and_config_independently() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let original_config = json!({"room": "ops"});
    let replacement_config = json!({"room": "eng", "notify": false});
    let channel = create_channel(
        &catalog,
        &tenant,
        "ops-room",
        ChannelKind::Slack,
        Some(&original_config),
    )
    .await;

    let renamed = catalog
        .channels()
        .update(
            &tenant.id,
            &channel.id,
            ChannelPatch {
                name: Some("eng-room"),
                kind: None,
                config: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "eng-room");
    assert_eq!(renamed.kind, ChannelKind::Slack);
    assert_eq!(renamed.config, original_config);

    let retyped = catalog
        .channels()
        .update(
            &tenant.id,
            &channel.id,
            ChannelPatch {
                name: None,
                kind: Some(ChannelKind::Teams),
                config: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(retyped.name, "eng-room");
    assert_eq!(retyped.kind, ChannelKind::Teams);
    assert_eq!(retyped.config, original_config);

    let reconfigured = catalog
        .channels()
        .update(
            &tenant.id,
            &channel.id,
            ChannelPatch {
                name: None,
                kind: None,
                config: Some(Some(&replacement_config)),
            },
        )
        .await
        .unwrap();
    assert_eq!(reconfigured.name, "eng-room");
    assert_eq!(reconfigured.kind, ChannelKind::Teams);
    assert_eq!(reconfigured.config, replacement_config);
}

#[tokio::test]
async fn channel_update_config_some_none_clears_to_empty_object() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let config = json!({"webhook": "https://example.test/hook"});
    let channel = create_channel(
        &catalog,
        &tenant,
        "webhook-ingest",
        ChannelKind::Webhook,
        Some(&config),
    )
    .await;

    let updated = catalog
        .channels()
        .update(
            &tenant.id,
            &channel.id,
            ChannelPatch {
                config: Some(None),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.config, json!({}));
}

#[tokio::test]
async fn channel_update_cross_tenant_returns_not_found_without_mutating() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let config = json!({"room": "ops"});
    let channel = create_channel(
        &catalog,
        &bob,
        "ops-room",
        ChannelKind::Slack,
        Some(&config),
    )
    .await;

    let err = catalog
        .channels()
        .update(
            &alice.id,
            &channel.id,
            ChannelPatch {
                name: Some("renamed"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));

    let fetched = catalog.channels().get(&bob.id, &channel.id).await.unwrap();
    assert_eq!(fetched.name, "ops-room");
    assert_eq!(fetched.config, config);
}

#[tokio::test]
async fn channel_update_duplicate_name_returns_conflict() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let first = create_channel(&catalog, &tenant, "alpha", ChannelKind::Slack, None).await;
    let _second = create_channel(&catalog, &tenant, "beta", ChannelKind::Teams, None).await;

    let err = catalog
        .channels()
        .update(
            &tenant.id,
            &first.id,
            ChannelPatch {
                name: Some("beta"),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::Conflict(_)));
}

#[tokio::test]
async fn channel_delete_removes_row_and_future_get_returns_not_found() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let channel = create_channel(&catalog, &tenant, "ops-room", ChannelKind::Slack, None).await;

    catalog
        .channels()
        .delete(&tenant.id, &channel.id)
        .await
        .unwrap();

    let err = catalog
        .channels()
        .get(&tenant.id, &channel.id)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));
}

#[tokio::test]
async fn channel_delete_cascades_agent_channels_but_leaves_agent_row() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let channel = create_channel(&catalog, &tenant, "ops-room", ChannelKind::Slack, None).await;
    let agent = create_agent(&catalog, &tenant, "assistant", slice::from_ref(&channel.id)).await;

    catalog
        .channels()
        .delete(&tenant.id, &channel.id)
        .await
        .unwrap();

    let fetched_agent = catalog.agents().get(&tenant.id, &agent.id).await.unwrap();
    assert_eq!(fetched_agent.id.as_str(), agent.id.as_str());
    let joined = catalog
        .channels()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert!(joined.is_empty());
}

#[tokio::test]
async fn channel_delete_cross_tenant_returns_not_found_without_deleting_visible_row() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let channel = create_channel(&catalog, &bob, "ops-room", ChannelKind::Slack, None).await;

    let err = catalog
        .channels()
        .delete(&alice.id, &channel.id)
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::NotFound(_)));

    let fetched = catalog.channels().get(&bob.id, &channel.id).await.unwrap();
    assert_eq!(fetched.id.as_str(), channel.id.as_str());
}

#[tokio::test]
async fn create_agent_with_same_tenant_channels_lists_joined_channels_in_created_order() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let first = create_channel(&catalog, &tenant, "alpha", ChannelKind::Slack, None).await;
    sleep(Duration::from_millis(2)).await;
    let second = create_channel(&catalog, &tenant, "beta", ChannelKind::Teams, None).await;
    let channel_ids = vec![first.id.clone(), second.id.clone()];
    let agent = create_agent(&catalog, &tenant, "assistant", &channel_ids).await;

    let joined = catalog
        .channels()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(
        channel_id_strings(&joined),
        vec![first.id.as_str().to_owned(), second.id.as_str().to_owned()]
    );
}

#[tokio::test]
async fn create_agent_with_foreign_tenant_channel_returns_validation_and_does_not_create_agent() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let foreign_channel = create_channel(&catalog, &bob, "foreign", ChannelKind::Slack, None).await;

    let err = catalog
        .agents()
        .create(
            &alice.id,
            NewAgent {
                name: "assistant",
                description: None,
                system_prompt: "prompt",
                model: "model",
                max_turns: None,
                max_tokens: None,
                memory_pool_id: None,
                skill_ids: &[],
                capabilities: &[],
                channel_ids: slice::from_ref(&foreign_channel.id),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::Validation(_)));

    let agents = catalog
        .agents()
        .list(&alice.id, PageRequest::default(), None)
        .await
        .unwrap();
    assert!(agents.items.is_empty());
}

#[tokio::test]
async fn update_agent_with_some_channel_ids_replaces_binding_atomically() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let first = create_channel(&catalog, &tenant, "alpha", ChannelKind::Slack, None).await;
    let second = create_channel(&catalog, &tenant, "beta", ChannelKind::Teams, None).await;
    let third = create_channel(&catalog, &tenant, "charlie", ChannelKind::Email, None).await;
    let initial_ids = vec![first.id.clone(), second.id.clone()];
    let agent = create_agent(&catalog, &tenant, "assistant", &initial_ids).await;
    let replacement_ids = vec![third.id.clone()];

    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                channel_ids: Some(&replacement_ids),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let joined = catalog
        .channels()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(
        channel_id_strings(&joined),
        vec![third.id.as_str().to_owned()]
    );
}

#[tokio::test]
async fn update_agent_with_channel_ids_none_leaves_existing_binding_unchanged() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let first = create_channel(&catalog, &tenant, "alpha", ChannelKind::Slack, None).await;
    let second = create_channel(&catalog, &tenant, "beta", ChannelKind::Teams, None).await;
    let channel_ids = vec![first.id.clone(), second.id.clone()];
    let agent = create_agent(&catalog, &tenant, "assistant", &channel_ids).await;

    catalog
        .agents()
        .update(
            &tenant.id,
            &agent.id,
            AgentPatch {
                model: Some("model-v2"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let joined = catalog
        .channels()
        .list_for_agent(&tenant.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(
        channel_id_strings(&joined),
        vec![first.id.as_str().to_owned(), second.id.as_str().to_owned()]
    );
}

#[tokio::test]
async fn update_agent_with_foreign_channel_returns_validation_and_keeps_existing_binding() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let local = create_channel(&catalog, &alice, "local", ChannelKind::Slack, None).await;
    let foreign = create_channel(&catalog, &bob, "foreign", ChannelKind::Teams, None).await;
    let agent = create_agent(&catalog, &alice, "assistant", slice::from_ref(&local.id)).await;
    let replacement_ids = vec![foreign.id.clone()];

    let err = catalog
        .agents()
        .update(
            &alice.id,
            &agent.id,
            AgentPatch {
                channel_ids: Some(&replacement_ids),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CatalogError::Validation(_)));

    let joined = catalog
        .channels()
        .list_for_agent(&alice.id, &agent.id)
        .await
        .unwrap();
    assert_eq!(
        channel_id_strings(&joined),
        vec![local.id.as_str().to_owned()]
    );
}

#[tokio::test]
async fn resolve_populates_channels_in_created_at_then_id_order() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    let first = create_channel(&catalog, &tenant, "alpha", ChannelKind::Slack, None).await;
    sleep(Duration::from_millis(2)).await;
    let second = create_channel(&catalog, &tenant, "beta", ChannelKind::Teams, None).await;
    let channel_ids = vec![first.id.clone(), second.id.clone()];
    create_agent(&catalog, &tenant, "assistant", &channel_ids).await;

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();

    assert_eq!(
        channel_id_strings(&resolved.channels),
        vec![first.id.as_str().to_owned(), second.id.as_str().to_owned()]
    );
}

#[tokio::test]
async fn resolve_for_agent_with_no_channels_returns_empty_vec() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;
    create_agent(&catalog, &tenant, "assistant", &[]).await;

    let resolved = catalog
        .agents()
        .resolve(&tenant.id, "assistant")
        .await
        .unwrap();

    assert!(resolved.channels.is_empty());
}

#[tokio::test]
async fn list_for_agent_cross_tenant_query_returns_empty_vec_instead_of_error() {
    let catalog = fresh();
    let alice = create_tenant(&catalog, "alice").await;
    let bob = create_tenant(&catalog, "bob").await;
    let bob_channel = create_channel(&catalog, &bob, "ops-room", ChannelKind::Slack, None).await;
    let agent = create_agent(
        &catalog,
        &bob,
        "assistant",
        slice::from_ref(&bob_channel.id),
    )
    .await;

    // Chosen behavior: a cross-tenant agent lookup is hidden as "no channels",
    // matching the repo surface instead of surfacing NotFound.
    let joined = catalog
        .channels()
        .list_for_agent(&alice.id, &agent.id)
        .await
        .unwrap();

    assert!(joined.is_empty());
}

#[tokio::test]
async fn channel_kind_variants_round_trip_exactly_through_create_and_get() {
    let catalog = fresh();
    let tenant = create_tenant(&catalog, "acme").await;

    for (name, kind) in [
        ("slack", ChannelKind::Slack),
        ("teams", ChannelKind::Teams),
        ("email", ChannelKind::Email),
        ("webhook", ChannelKind::Webhook),
        ("manual", ChannelKind::Manual),
    ] {
        let created = create_channel(&catalog, &tenant, name, kind, None).await;
        let fetched = catalog
            .channels()
            .get(&tenant.id, &created.id)
            .await
            .unwrap();
        assert_eq!(fetched.kind, kind);
    }
}
