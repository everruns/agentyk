# VHS tape for the codenko demo GIF.
#
#   cargo build --release -p codenko
#   PATH="$PWD/target/release:$PATH" OPENAI_API_KEY=... vhs examples/codenko/demo.tape
#   gifsicle -O3 --lossy=60 --colors 64 -b examples/codenko/docs/demo.gif
#
# The gifsicle pass is not optional if you intend to commit the result: at 2x
# density vhs writes ~1.4 MB, and a terminal recording has few enough colors
# that 64 with lossy=60 is visually indistinguishable at 557 KB.
#
# Recorded at 2x pixel density and displayed at half width (`<img width="880">`
# in the README), following tuika's own demo convention. A GIF wider than the
# README column gets resampled by a fractional factor and looks soft; an exact
# 2:1 downscale stays crisp, and also holds up on HiDPI screens. Width 1760 with
# font 28 and matching padding gives ~100 columns, which is what the transcript
# is laid out for.
#
# The recording is a real agent run against a real model, so the exact wording
# varies between takes; the sleeps are sized for the slowest step.
#
# `--reasoning-effort none` is not a stylistic choice: OpenAI's gpt-5.6 family
# refuses function tools on chat completions at any other level, and codenko is
# nothing but function tools. Without it the first turn ends in a `turn failed`
# notice carrying that exact message from the API.
#
# Two things keep a take from going sideways. The first prompt says not to
# change anything, because an approval prompt ignores every key but y/n — if the
# model reached for `write_file` in the first turn, the second prompt would be
# typed into that prompt instead of the composer. And each approval is answered
# with `y` followed by `Backspace`, so a stray `y` that arrives after the prompt
# has closed is typed into the composer and then removed, which keeps the take
# usable when the model asks for one approval fewer.

Output examples/codenko/docs/demo.gif

Require codenko

Set Shell bash
Set FontSize 28
Set Width 1760
Set Height 1150
Set Padding 28
Set Theme "TokyoNight"
Set Framerate 24
Set TypingSpeed 45ms

# Set up a scratch workspace with a small, obvious bug: fizzbuzz checks %3 and
# %5 before %15, so 15 prints "Fizz".
Hide
Type `cd "$(mktemp -d)" && printf 'def fizzbuzz(n):\n    if n %% 3 == 0:\n        return "Fizz"\n    if n %% 5 == 0:\n        return "Buzz"\n    if n %% 15 == 0:\n        return "FizzBuzz"\n    return str(n)\n\n\nfor i in range(1, 16):\n    print(fizzbuzz(i))\n' > fizz.py && clear`
Enter
Sleep 1s
Show

Type "codenko --provider openai --model gpt-5.6-terra --reasoning-effort none"
Enter
Sleep 2500ms

# First turn: a question, explicitly read-only, so nothing is gated.
Type "what does fizz.py print for 15, and is that right? don't change anything yet"
Enter
Sleep 20s

# Second turn: the fix plus a verification run — both gated.
Type "now fix it and run the file to confirm"
Enter
Sleep 12s
Type "y"
Backspace
Sleep 12s
Type "y"
Backspace
Sleep 16s

# End on the finished transcript rather than on the shell after quitting.
Sleep 3s
