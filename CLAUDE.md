# kip

Every user-facing capability of kip must also be reachable over MCP. When you
add or change a feature (sessions, windows, groups, settings, anything a user
can do with the mouse), add or update the matching op in `src/app_ctl.rs`
(`App::ctl`) and its tool in `src/mcp.rs` (`tools()`), and the tool table in
README's "MCP" section. Keep tool descriptions short: the whole tool list is
loaded into Claude's context (now ~7.6 KB / ~2.1k tokens; recheck the README
figure when tools change).

eframe is vendored in `vendor/eframe` with a fix (grep "kip patch") and pinned
to `=0.35.0`. Bumping egui/eframe drops the patch without an error: first check
whether upstream fixed minimized windows freezing the UI, else port the patch.
