# VHS tape for the OSBB live-provider demo.
#
#   cargo build --release -p osbb
#   PATH="$PWD/target/release:$PATH" OPENAI_API_KEY=... vhs examples/osbb/demo.tape
#   gifsicle -O3 --lossy=60 --colors 64 -b examples/osbb/docs/demo.gif
#
# Five co-owners share the conversation; two of them report the same night
# noise from different apartments and the association answers both.
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

Type "osbb --people Olena,Petro,Iryna,Mykola,Sofia --actor Olena --building 'Sadova 12' --agent-name Manager --model gpt-5.6-terra --reasoning-effort none"
Enter
Sleep 2500ms

Type "Apartment 41 has been playing music past midnight all week. I am in 37, directly below, and nobody can sleep."
Enter
Sleep 18s

Type "/as Petro"
Enter
Sleep 1s
Type "The same noise reaches 45 across the landing - Tuesday and Thursday, around 00:30. Olena is not imagining it."
Enter
Sleep 18s

Type "/as Iryna"
Enter
Sleep 1s
Type "Manager, what happens now, and does the rest of the house have to vote on anything?"
Enter
Sleep 18s

Sleep 3s
