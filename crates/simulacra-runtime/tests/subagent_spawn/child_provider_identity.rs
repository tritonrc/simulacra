#[tokio::test]
async fn each_child_provider_is_built_with_its_own_agent_id() {
    let built_for = Arc::new(Mutex::new(Vec::<AgentId>::new()));
    let built_for_factory = Arc::clone(&built_for);
    let harness = child_cell_identity_harness_with_provider_factory(
        None,
        None,
        Arc::new(move |spawn, _kind, _model| {
            built_for_factory
                .lock()
                .expect("provider factory capture")
                .push(spawn.agent_id.clone());
            Ok(Box::new(completing_native_child_provider()))
        }),
    );

    let first = spawn_native_child_and_join(&harness, "first task", "first_child").await;
    let second = spawn_native_child_and_join(&harness, "second task", "second_child").await;

    assert_ne!(first, second);
    assert_eq!(
        *built_for.lock().expect("provider factory capture"),
        vec![first, second]
    );

    finish_child_cell_identity_harness(harness).await;
}
