# VHS tape — records the live status dashboard (ma-demo status) as a GIF.
# Run against an already-provisioned environment; the dashboard re-probes the
# real KIND resources every 2s (watch the spinner advance), then we quit with q.
#
# Render with:  vhs docs/dashboard.tape   (outputs docs/dashboard.gif)
# Requires a running env at $WS (set below) — i.e. after `ma-demo run`.

Output "dashboard.gif"

Set Shell "bash"
Set FontSize 14
Set Width 1240
Set Height 780
Set Padding 14
Set Theme "Catppuccin Mocha"
Set TypingSpeed 50ms

Hide
Type "cd /Users/akurait/demo/opensearch-migrations-demo"
Enter
Type "clear"
Enter
Show
Sleep 500ms
Type "./target/release/ma-demo status --workspace /tmp/ma-dash/migration-demo-workspace"
Sleep 700ms
Enter

# Let the dashboard live + auto-refresh for several ticks (spinner advances,
# counts/health update in real time).
Sleep 12s

# Quit cleanly.
Type "q"
Sleep 1.2s
