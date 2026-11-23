# Requires vhs and kioku on PATH. Creates a disposable Git repository.
Output /tmp/kioku-demo.gif
Set Width 1200
Set Height 700
Set FontSize 16
Set Theme {"background":"#1c1c1c","foreground":"#e6e6e6"}
Type "mkdir -p /tmp/kioku-vhs-demo && cd /tmp/kioku-vhs-demo && git init -q"
Enter
Type "export KIOKU_DIR=/tmp/kioku-vhs-demo/cache"
Enter
Type "kioku run -- echo checks passed"
Enter
Sleep 1
Type "kioku run -- echo checks passed"
Enter
Sleep 1
Type "kioku status"
Enter
Sleep 1
