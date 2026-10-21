# kip

Every user-facing capability of kip must also be reachable over MCP. When you
add or change a feature (sessions, windows, groups, settings, anything a user
can do with the mouse), add or update the matching op in `src/app_ctl.rs`
(`App::ctl`) and its tool in `src/mcp.rs` (`tools()`), and the tool table in
README's "MCP" section. Keep tool descriptions short: the whole tool list is
loaded into Claude's context (now ~6.1 KB / ~1.7k tokens; recheck the README
figure when tools change).
