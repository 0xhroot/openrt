//! Phase 11 Track H (M11.6/M11.7): cross-surface shared state.
//!
//! One `Runtime` + one store; the TUI controller and the WebUI command
//! palette are pure views over it. Anything written through one surface is
//! immediately visible through the other — there is no UI-local database
//! and no second authority.
//!
//! NOTE: the palette dispatches synchronously against a tokio handle, so
//! this test drives every async step with `block_on` from the test thread
//! and calls `dispatch` only outside those blocks (never within a runtime).

use std::sync::Arc;

use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_webui::{CommandPalette, DispatchRequest, PaletteOutcome};

fn shared_runtime() -> Arc<Runtime> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let app = rt.block_on(async {
        RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::echo("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap()
    });
    // Leak the helper runtime for the duration of the test: the palette's
    // handle must stay alive for its synchronous dispatches.
    std::mem::forget(rt);
    app
}

fn dispatch_detail(palette: &mut CommandPalette, req: DispatchRequest) -> serde_json::Value {
    match palette.dispatch(&req).expect("dispatch") {
        PaletteOutcome::Completed { detail, .. } => {
            serde_json::from_str(&detail).expect("detail is JSON")
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// A room created through the WebUI palette is visible in the TUI (and the
/// TUI's steer reaches the palette) — same ids, same records, both ways.
#[test]
fn h1_palette_room_visible_in_tui_and_steer_flows_both_ways() {
    let app = shared_runtime();
    let helper = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let handle = helper.handle().clone();

    // --- TUI surface over the shared runtime. ---
    let mut tui = helper
        .block_on(openrt_tui::App::new(app.clone()))
        .expect("tui app");
    helper
        .block_on(tui.create_workspace("acct/shared", "Shared", "cross-surface"))
        .expect("workspace create");

    // --- WebUI palette over the SAME runtime. ---
    let improve = openrt_selfimprove::ImproveStore::new(&["alice".to_string()], false);
    let mut palette = CommandPalette::with_runtime(improve, app.clone(), handle);
    let token = palette.issue_token("alice");

    // Palette creates a room in the TUI-created workbench (fail-closed
    // foundation: the workbench must exist — it does, because the TUI made
    // it through the shared store).
    let value = dispatch_detail(
        &mut palette,
        DispatchRequest::new("room.create")
            .with_arg("workbench", "acct/shared")
            .with_arg("title", "Palette room")
            .with_auth_token(&token),
    );
    let palette_room_id = value["id"].as_str().unwrap().to_string();

    // TUI refresh sees the palette-created room (same store, same id).
    helper.block_on(tui.refresh()).unwrap();
    assert!(
        tui.ops_rooms.iter().any(|r| r.id == palette_room_id),
        "TUI must list the palette-created room: {:?}",
        tui.ops_rooms
    );
    assert_eq!(tui.ops_rooms.len(), 1);

    // TUI steers the palette-created room (append-only through the runtime).
    tui.set_ops_tab(openrt_tui::OpsTab::Rooms);
    tui.room_steer_begin();
    for c in "from the tui".chars() {
        if let Some(buf) = tui.steer_input.as_mut() {
            buf.push(c);
        }
    }
    helper.block_on(tui.room_steer_submit());
    assert!(tui.steer_input.is_none(), "input consumed");
    let room = helper.block_on(tui.runtime.room(&palette_room_id)).unwrap();
    assert_eq!(room.steer.len(), 1);
    assert_eq!(room.steer[0].operator, "tui");
    assert_eq!(room.steer[0].input, "from the tui");

    // Palette show lists the TUI-originated steer directive (other way).
    let value = dispatch_detail(
        &mut palette,
        DispatchRequest::new("room.show").with_arg("id", &palette_room_id),
    );
    assert_eq!(value["steer"].as_array().map(Vec::len), Some(1), "{value}");
    assert_eq!(value["steer"][0]["operator"], "tui");
    assert_eq!(value["steer"][0]["input"], "from the tui");

    // Palette steers back; the TUI refresh sees it immediately.
    let out = palette
        .dispatch(
            &DispatchRequest::new("room.steer")
                .with_arg("id", &palette_room_id)
                .with_arg("input", "from the palette")
                .with_auth_token(&token),
        )
        .expect("palette room.steer");
    assert!(matches!(out, PaletteOutcome::Completed { .. }));
    helper.block_on(tui.refresh()).unwrap();
    let row = tui
        .ops_rooms
        .iter()
        .find(|r| r.id == palette_room_id)
        .expect("room still listed");
    assert_eq!(row.steer, 2, "both directives visible in the TUI view");

    // Status change via palette → TUI view reflects it (same record).
    let out = palette
        .dispatch(
            &DispatchRequest::new("room.status")
                .with_arg("id", &palette_room_id)
                .with_arg("status", "paused")
                .with_auth_token(&token),
        )
        .expect("palette room.status");
    assert!(matches!(out, PaletteOutcome::Completed { .. }));
    helper.block_on(tui.refresh()).unwrap();
    let row = tui
        .ops_rooms
        .iter()
        .find(|r| r.id == palette_room_id)
        .unwrap();
    assert_eq!(row.status, "paused", "status is shared, not UI-local");

    // workbench.info (palette) and the TUI row count agree — one model.
    let info = dispatch_detail(&mut palette, DispatchRequest::new("workbench.info"));
    assert_eq!(info["rooms"], tui.ops_rooms.len() as u64, "{info}");
    assert_eq!(info["workspaces"], tui.workspaces.len() as u64, "{info}");

    // Room agent assignment (M11.4) through the palette reaches the store
    // the TUI renders — role configuration, one authority.
    let out = palette
        .dispatch(
            &DispatchRequest::new("room.agent")
                .with_arg("id", &palette_room_id)
                .with_arg("agent", "agent-h")
                .with_arg("role", "verifier")
                .with_auth_token(&token),
        )
        .expect("palette room.agent");
    assert!(matches!(out, PaletteOutcome::Completed { .. }));
    helper.block_on(tui.refresh()).unwrap();
    let row = tui
        .ops_rooms
        .iter()
        .find(|r| r.id == palette_room_id)
        .unwrap();
    assert_eq!(row.agents, 1, "agent count shared across surfaces");
}
