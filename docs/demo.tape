# Requires vhs, cc, git and kioku on PATH, plus KIOKU_STRACE pointing at the
# pinned Debian strace 6.1 (README.md, "Pinned tracer"). Run from the
# repository root. The script prints its own transcript.
Output /tmp/kioku-demo.gif
Set Width 1200
Set Height 700
Set FontSize 16
Set Theme {"background":"#1c1c1c","foreground":"#e6e6e6"}
Type "sh docs/demo.sh"
Enter
Sleep 5
