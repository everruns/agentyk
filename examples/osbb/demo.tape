# VHS tape for the OSBB live-provider demo.
#
#   cargo build --release -p osbb
#   PATH="$PWD/target/release:$PATH" OPENAI_API_KEY=... vhs examples/osbb/demo.tape
#   gifsicle -O3 --lossy=60 --colors 64 -b examples/osbb/docs/demo.gif
#
# This is a real gpt-5.6-terra run. Sleeps allow for provider variance.

Output examples/osbb/docs/demo.gif

Require osbb

Set Shell bash
Set FontSize 28
Set Width 1760
Set Height 1050
Set Padding 28
Set Theme "TokyoNight"
Set Framerate 24
Set TypingSpeed 42ms

Type "osbb --people Ada,Grace --actor Ada --agent-name Archivist --model gpt-5.6-terra --reasoning-effort none"
Enter
Sleep 2500ms

Type "Grace and I need to pick a launch day. I prefer Tuesday because marketing is ready."
Enter
Sleep 18s

Type "/as Grace"
Enter
Sleep 1s
Type "Ada, Wednesday is safer because support is fully staffed. Archivist, summarize the tradeoff."
Enter
Sleep 18s

Type "/as Ada"
Enter
Sleep 1s
Type "That support coverage decides it for me. Record that we agree on Wednesday."
Enter
Sleep 18s

Sleep 3s
