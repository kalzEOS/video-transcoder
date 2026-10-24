#[test]
fn close_requested_handler_can_destroy_the_window() {
    let capability = include_str!("../capabilities/default.json");
    let config: serde_json::Value = serde_json::from_str(capability).unwrap();
    let permissions = config["permissions"].as_array().unwrap();

    // Tauri's Window.onCloseRequested closes an unblocked window by calling
    // destroy(), including when the user clicks X on an idle window.
    assert!(permissions.iter().any(|p| p == "core:window:allow-destroy"));
}
