# VHS tape for the OpenAI demo GIF — the same scenario as `demo.tape`, on the
# other provider, so the two recordings are directly comparable.
#
#   cargo build --release -p codenko
#   PATH="$PWD/target/release:$PATH" OPENAI_API_KEY=... vhs examples/codenko/demo-openai.tape
#
# `--reasoning-effort none` is not a stylistic choice: OpenAI's gpt-5.6 family
# refuses function tools on chat completions at any other level, and codenko is
# nothing but function tools. Without it the first turn ends in a `turn failed`
# notice carrying that exact message from the API.
#
# See `demo.tape` for why the first prompt says "don't change anything yet" and
# why each `y` is followed by a `Backspace`.

Output examples/codenko/docs/demo-openai.gif

Require codenko

Set Shell bash
Set FontSize 14
Set Width 1180
Set Height 700
Set Padding 14
Set Theme "TokyoNight"
Set TypingSpeed 45ms

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
