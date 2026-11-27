# Records demo/lyst.gif. Regenerate from the repository root with:
#
#   cargo build --release && vhs demo/demo.tape
#
# Needs VHS (https://github.com/charmbracelet/vhs): `brew install vhs`.

Output demo/lyst.gif

Require cargo

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 640
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 80ms
Set PlaybackSpeed 1.0

# Use the freshly built binary, and no LESSOPEN preprocessor, so the
# recording looks the same on every machine. The samples are copied to a
# neutral directory so the status bar doesn't show the recorder's own paths.
Hide
Type "export PATH=$PWD/target/release:$PATH LYST_OPEN= PS1='$ '; rm -rf /tmp/lyst-demo; mkdir /tmp/lyst-demo; cp -R samples /tmp/lyst-demo/; cd /tmp/lyst-demo; clear"
Enter
Show

# File browser
Type "lyst samples"
Sleep 400ms
Enter
Sleep 1.5s
Down
Sleep 400ms
Down
Sleep 600ms

# / then the start of a name jumps to people.csv; Enter opens it as a table.
Type "/peo"
Sleep 800ms
Enter
Sleep 2.5s

# Right selects the age column; > sorts by it (numbers by value). More
# Right scrolls sideways, with the name column pinned.
Right
Sleep 900ms
Type ">"
Sleep 2s
Right
Sleep 700ms
Right
Sleep 700ms
Right
Sleep 1.5s

# Back to the list, then a VCF: metadata as plain text, header pinned.
Type "x"
Sleep 800ms
Type "/var"
Sleep 800ms
Enter
Sleep 2.5s
PageDown
Sleep 1.5s

# & shows only matching lines; then sort them by QUAL, highest first.
Type "&"
Sleep 400ms
Type "LowQual"
Sleep 500ms
Enter
Sleep 2s
Right
Sleep 300ms
Right
Sleep 300ms
Right
Sleep 300ms
Right
Sleep 300ms
Right
Sleep 800ms
Type "<"
Sleep 2.2s

# Syntax highlighting, toggled with S; H shows a hex dump.
Type "x"
Sleep 800ms
Type "/peaks.r"
Sleep 800ms
Enter
Sleep 2.5s
PageDown
Sleep 1.5s
Type "s"
Sleep 1.5s
Type "s"
Sleep 1.2s
Type "H"
Sleep 2.2s
Type "H"
Sleep 1s

Type "q"
Sleep 1s

Hide
Type "cd - >/dev/null; rm -rf /tmp/lyst-demo"
Enter
