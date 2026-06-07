# VHS tape — records the migration-assistant-demo wizard walkthrough as a GIF.
# Drives the real `ma-demo plan` binary (collects answers interactively, prints
# the plan, provisions nothing — a clean, side-effect-free showcase of the TUI).
#
# Render with:  vhs docs/demo.tape   (outputs docs/demo.gif)

Output "demo.gif"

Set Shell "bash"
Set FontSize 16
Set Width 1180
Set Height 720
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms
Set PlaybackSpeed 1.0

# --- launch the wizard (plan mode = no Docker side effects) ---
Hide
Type "cd /Users/akurait/demo/opensearch-migrations-demo && rm -rf /tmp/gif-ws"
Enter
Type "clear"
Enter
Show
Sleep 600ms
Type "./target/release/ma-demo plan --workspace /tmp/gif-ws"
Sleep 800ms
Enter
Sleep 1.8s

# Q1 Where → Local (already on Local; confirm)
Sleep 1.2s
Enter
Sleep 1.2s

# Q2 Source engine → OpenSearch (Down once, confirm)
Down
Sleep 900ms
Enter
Sleep 1.2s

# Q3 Source version → 2.19.0 (top option; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q4 Source plugins (multi) → toggle repository-s3, then confirm
Sleep 700ms
Space
Sleep 900ms
Enter
Sleep 1.2s

# Q5 Snapshot storage → LocalStack (top; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q6 Provision target? → Provision (top; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q7 Target kind → Local OpenSearch (top; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q8 Target version → 3.3.0 (top; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q9 Client apps (multi) → toggle Locust + sample app, confirm
Sleep 700ms
Space
Sleep 700ms
Down
Sleep 500ms
Space
Sleep 900ms
Enter
Sleep 1.2s

# Q10 Seed data? → Yes (default; confirm)
Sleep 900ms
Enter
Sleep 1.2s

# Q11 MA handoff → Deploy locally to KIND (top; confirm)
Sleep 900ms
Enter
Sleep 2.5s

# --- the plan prints; hold on it ---
Sleep 4s
