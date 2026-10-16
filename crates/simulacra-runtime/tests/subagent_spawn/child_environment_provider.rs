// A host hook supplies each native child's filesystem and file presenter. The
// hook is asked once per spawn with that child's id, so these tests keep the
// children apart by the id baked into their filesystems and presenters.
// The tests live in their own module so `cargo test child_environment_provider`
// selects them even though this file is included into the shared test crate.
mod child_environment_provider {
    use super::*;
    include!("child_environment_support.rs");

    #[tokio::test]
    async fn each_spawn_asks_the_provider_with_that_childs_id_and_uses_its_filesystem() {
        let asked = Arc::new(Mutex::new(Vec::<AgentId>::new()));
        let asked_for_hook = Arc::clone(&asked);
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_provider(move |spawn| {
                asked_for_hook.lock().unwrap().push(spawn.agent_id.clone());
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: child_env_vfs_with_marker(&child_env_marker_for(spawn)),
                    file_presenter: None,
                })
            })),
            Arc::clone(&seen),
        );

        let (first_id, first) = child_env_spawn_and_join(&harness, "env_alpha").await;
        let (second_id, second) = child_env_spawn_and_join(&harness, "env_beta").await;

        assert_ne!(first_id, second_id);
        assert_eq!(first["status"], "completed");
        assert_eq!(second["status"], "completed");
        // Once per spawn, with the id the spawn acknowledged, in spawn order.
        assert_eq!(
            *asked.lock().unwrap(),
            vec![first_id.clone(), second_id.clone()]
        );
        // Each child read the filesystem built for its own id, not the factory's
        // shared one and not its sibling's.
        assert_eq!(
            child_env_tool_messages(&seen),
            vec![
                format!("for {}", first_id.0),
                format!("for {}", second_id.0)
            ]
        );

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn the_childs_file_read_presents_through_the_supplied_presenter() {
        let presenter =
            ChildEnvPresenter::answering(simulacra_types::ToolOutput::success("shown by host"));
        let presenter_for_hook = Arc::clone(&presenter);
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_provider(move |_spawn| {
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: child_env_vfs_with_marker("raw bytes"),
                    file_presenter: Some(
                        Arc::clone(&presenter_for_hook) as Arc<dyn simulacra_tool::FilePresenter>
                    ),
                })
            })),
            Arc::clone(&seen),
        );

        child_env_spawn_and_join(&harness, "env_presenter").await;

        assert_eq!(child_env_tool_messages(&seen), vec!["shown by host"]);
        // The registry passes the model's request and the whole file through.
        assert_eq!(
            *presenter.seen.lock().unwrap(),
            vec![(child_env_marker_request(), b"raw bytes".to_vec())]
        );

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn a_provider_error_fails_that_spawn_with_the_providers_message() {
        let asked = Arc::new(Mutex::new(Vec::<AgentId>::new()));
        let asked_for_hook = Arc::clone(&asked);
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_provider(move |spawn| {
                asked_for_hook.lock().unwrap().push(spawn.agent_id.clone());
                Err(RuntimeError::Sandbox("no filesystem".into()))
            })),
            Arc::clone(&seen),
        );

        let (child_id, joined) = child_env_spawn_and_join(&harness, "env_failure").await;

        assert_eq!(joined["status"], "failed");
        assert!(
            joined["message"]
                .as_str()
                .is_some_and(|message| message.contains("no filesystem")),
            "the failure should carry the hook's message, got {joined}"
        );
        assert_eq!(*asked.lock().unwrap(), vec![child_id]);
        // The child never ran: it neither fell back to the factory filesystem nor
        // reached its model.
        assert!(seen.lock().unwrap().is_empty());

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn without_a_provider_children_share_the_factory_filesystem() {
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(None, Arc::clone(&seen));

        let (_first_id, first) = child_env_spawn_and_join(&harness, "env_shared_alpha").await;
        let (_second_id, second) = child_env_spawn_and_join(&harness, "env_shared_beta").await;

        assert_eq!(first["status"], "completed");
        assert_eq!(second["status"], "completed");
        assert_eq!(
            child_env_tool_messages(&seen),
            vec!["factory-shared", "factory-shared"]
        );

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn a_slow_environment_for_one_child_does_not_hold_up_another_spawn() {
        let (release_slow, slow_gate) = tokio::sync::oneshot::channel::<()>();
        let slow_gate = Mutex::new(Some(slow_gate));
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_gated_provider(
                move |spawn| {
                    // Keyed by task text, not call order: the two child tasks may
                    // reach the hook in either order.
                    if spawn.task == "slow child" {
                        slow_gate.lock().unwrap().take()
                    } else {
                        None
                    }
                },
                |spawn| {
                    Ok(simulacra_runtime::ChildFsEnvironment {
                        vfs: child_env_vfs_with_marker(&child_env_marker_for(spawn)),
                        file_presenter: None,
                    })
                },
            )),
            Arc::clone(&seen),
        );

        let slow_id = child_env_spawn(&harness, "slow child", "env_slow").await;
        let fast_id = child_env_spawn(&harness, "fast child", "env_fast").await;
        // A timeout makes a regression fail instead of hanging the suite.
        let fast = tokio::time::timeout(Duration::from_secs(5), child_env_join(&harness, &fast_id))
            .await
            .expect("the fast child must finish while the slow environment is still pending");

        assert_eq!(fast["status"], "completed");
        // The slow child is still waiting for its environment: it has not reached
        // its model, only the fast child's two requests have been seen.
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(
            child_env_tool_messages(&seen),
            vec![format!("for {}", fast_id.0)]
        );

        release_slow
            .send(())
            .expect("the slow hook should be waiting");
        let slow = tokio::time::timeout(Duration::from_secs(5), child_env_join(&harness, &slow_id))
            .await
            .expect("the slow child must finish once its environment is released");

        assert_eq!(slow["status"], "completed");
        assert_eq!(
            child_env_tool_messages(&seen),
            vec![format!("for {}", fast_id.0), format!("for {}", slow_id.0)]
        );

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn each_child_gets_the_presenter_its_own_environment_supplied() {
        let presenters = Arc::new(Mutex::new(std::collections::HashMap::<
            String,
            Arc<ChildEnvPresenter>,
        >::new()));
        let presenters_for_hook = Arc::clone(&presenters);
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_provider(move |spawn| {
                let presenter =
                    ChildEnvPresenter::answering(simulacra_types::ToolOutput::success("presented"));
                presenters_for_hook
                    .lock()
                    .unwrap()
                    .insert(spawn.agent_id.0.clone(), Arc::clone(&presenter));
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: child_env_vfs_with_marker(&child_env_marker_for(spawn)),
                    file_presenter: Some(presenter as Arc<dyn simulacra_tool::FilePresenter>),
                })
            })),
            Arc::clone(&seen),
        );

        let (first_id, _) = child_env_spawn_and_join(&harness, "env_presenter_alpha").await;
        let (second_id, _) = child_env_spawn_and_join(&harness, "env_presenter_beta").await;

        // Take the presenters out under the lock so no guard lives across the
        // await below.
        let (count, first, second) = {
            let presenters = presenters.lock().unwrap();
            (
                presenters.len(),
                Arc::clone(&presenters[&first_id.0]),
                Arc::clone(&presenters[&second_id.0]),
            )
        };
        assert_eq!(count, 2);
        // Each presenter saw exactly one request: its own child's read of its own
        // filesystem, never its sibling's.
        for (id, presenter) in [(&first_id, first), (&second_id, second)] {
            assert_eq!(
                *presenter.seen.lock().unwrap(),
                vec![(
                    child_env_marker_request(),
                    format!("for {}", id.0).into_bytes()
                )]
            );
        }

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn provider_content_from_the_presenter_reaches_the_childs_next_model_request() {
        let image = simulacra_types::ProviderContentBlock {
            provider: "anthropic".into(),
            value: serde_json::json!({
                "type": "image",
                "source": { "type": "base64", "media_type": "image/png", "data": "AA==" }
            }),
        };
        let presenter = ChildEnvPresenter::answering(simulacra_types::ToolOutput {
            provider_content: vec![image.clone()],
            ..simulacra_types::ToolOutput::success("an image")
        });
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness(
            Some(child_env_provider(move |_spawn| {
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: child_env_vfs_with_marker("png bytes"),
                    file_presenter: Some(
                        Arc::clone(&presenter) as Arc<dyn simulacra_tool::FilePresenter>
                    ),
                })
            })),
            Arc::clone(&seen),
        );

        child_env_spawn_and_join(&harness, "env_image").await;

        let results = child_env_tool_results(&seen);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "an image");
        // The block rides on the tool result itself, so the model sees the image.
        assert_eq!(results[0].provider_content, vec![image]);

        finish_child_cell_identity_harness(harness).await;
    }

    #[tokio::test]
    async fn child_writes_land_in_the_filesystem_the_host_supplied() {
        let host_vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let host_vfs_for_hook = Arc::clone(&host_vfs);
        let factory_vfs: Arc<dyn VirtualFs> = Arc::new(MemoryFs::new());
        let seen: ChildEnvRequests = Arc::default();
        let harness = child_env_harness_with_script(
            Some(child_env_provider(move |_spawn| {
                Ok(simulacra_runtime::ChildFsEnvironment {
                    vfs: Arc::clone(&host_vfs_for_hook),
                    file_presenter: None,
                })
            })),
            Arc::clone(&factory_vfs),
            |child_index| {
                child_env_call_then_finish(
                    "file_write",
                    serde_json::json!({
                        "path": format!("/workspace/out-{child_index}.txt"),
                        "content": "x"
                    }),
                )
            },
            Arc::clone(&seen),
        );

        let (_first_id, first) = child_env_spawn_and_join(&harness, "env_write_alpha").await;
        let (_second_id, second) = child_env_spawn_and_join(&harness, "env_write_beta").await;

        assert_eq!(first["status"], "completed");
        assert_eq!(second["status"], "completed");
        assert_eq!(host_vfs.read("/workspace/out-0.txt").unwrap(), b"x");
        assert_eq!(host_vfs.read("/workspace/out-1.txt").unwrap(), b"x");
        // Nothing leaked into the filesystem the factory was built with.
        assert!(!factory_vfs.exists("/workspace/out-0.txt"));
        assert!(!factory_vfs.exists("/workspace/out-1.txt"));

        finish_child_cell_identity_harness(harness).await;
    }
}
