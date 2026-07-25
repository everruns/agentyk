# VHS tape for the codenko demo GIF.
#
#   cargo build --release -p codenko
#   PATH="$PWD/target/release:$PATH" ANTHROPIC_API_KEY=... vhs examples/codenko/demo.tape
#
# The recording is a real agent run against a real model, so the exact wording
# varies between takes; the sleeps are sized for the slowest step.
#
# Two things keep a take from going sideways. The first prompt says not to
# change anything, because an approval prompt ignores every key but y/n — if
# the model reached for `write_file` in the first turn, the second prompt would
# be typed into that prompt instead of the composer. And each approval is
# answered with `y` followed by `Backspace`, so a stray `y` that arrives after
# the prompt has closed is typed into the composer and then removed, which
# keeps the take usable when the model asks for one approval fewer.

Output examples/codenko/docs/demo.gif

Require codenko

Set Shell bash
Set FontSize 14
Set Width 1180
Set Height 700
Set Padding 14
Set Theme "TokyoNight"
Set TypingSpeed 45ms

# Set up a scratch workspace with a small, obvious bug: fizzbuzz checks %3 and
# %5 before %15, so 15 prints "Fizz".
Hide
Type `cd "$(mktemp -d)" && printf 'def fizzbuzz(n):\n    if n %% 3 == 0:\n        return "Fizz"\n    if n %% 5 == 0:\n        return "Buzz"\n    if n %% 15 == 0:\n        return "FizzBuzz"\n    return str(n)\n\n\nfor i in range(1, 16):\n    print(fizzbuzz(i))\n' > fizz.py && clear`
Enter
Sleep 1s
Show

Type "codenko"
Enter
Sleep 2500ms

# First turn: a question, explicitly read-only, so nothing is gated.
Type "what does fizz.py print for 15, and is that right? don't change anything yet"
Enter
Sleep 30s

# Second turn: the fix plus a verification run — both gated.
Type "now fix it and run the file to confirm"
Enter
Sleep 15s
Type "y"
Backspace
Sleep 15s
Type "y"
Backspace
Sleep 18s

# End on the finished transcript rather than on the shell after quitting.
Sleep 3s
