# VHS tape for the README demo GIF (https://github.com/charmbracelet/vhs).
#
# Render from a small scratch repo containing a planted bug (see
# docs/ROADMAP.md v0.9). Assumes `rift` is on PATH and an Ollama server
# with a tools-capable model is reachable:
#
#   cd <demo-repo> && vhs /path/to/rift/docs/assets/demo.tape
#   mv rift-demo.gif <rift>/docs/assets/demo.gif

Output rift-demo.gif

Set FontSize 15
Set Width 1220
Set Height 720
Set Padding 12
Set TypingSpeed 35ms
Set WaitTimeout 120s
Set PlaybackSpeed 1.5

Type "rift"
Enter
Sleep 3s

Type "Fix the bug in stats.py: mean([2, 4, 6]) must print 4.0. Verify by running it."
Sleep 1s
Enter

# Track the agent instead of guessing sleeps: rift's status line renders
# "done — N steps · ..." when the turn finishes.
Wait+Screen /done — /
Sleep 3s

Type "/stats"
Sleep 500ms
Enter
Sleep 4s

Type "/quit"
Sleep 500ms
Enter
Sleep 1s
