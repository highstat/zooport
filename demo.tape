# zooport demo — recorded with VHS (https://github.com/charmbracelet/vhs)
#
# Prerequisites:
#   brew install vhs cloudflared
#   cargo build --release
#
# Record:
#   vhs demo.tape
#
# This produces demo.gif in the repository root.

Require python3
Require cloudflared

Output demo.gif

Set Shell bash
Set FontSize 16
Set Width 1200
Set Height 720
Set Padding 24
Set Theme "Catppuccin Mocha"
Set WindowBar Colorful
Set CursorBlink false
Set TypingSpeed 45ms
Set PlaybackSpeed 1.25
Set Framerate 30

# ---------------------------------------------------------------------------
# Hidden setup: a few realistic local services, then launch zooport.
# ---------------------------------------------------------------------------
Hide
Type "python3 -m http.server 3000 >/dev/null 2>&1 &"
Enter
Type "python3 -m http.server 5173 >/dev/null 2>&1 &"
Enter
Type "python3 -m http.server 8080 >/dev/null 2>&1 &"
Enter
Type "clear"
Enter
Type "./target/release/zooport"
Enter
Wait+Screen /ZOOPORT/
Sleep 1s
Show

# ---------------------------------------------------------------------------
# Navigation and live filtering.
# ---------------------------------------------------------------------------
Sleep 800ms
Down
Sleep 200ms
Down
Sleep 350ms

Type "/"
Sleep 200ms
Type "python:"
Sleep 900ms
Enter
Sleep 600ms

# ---------------------------------------------------------------------------
# Multi-select two rows to show the bulk-action affordance. The filter stays
# active so the tunnel targets one of the demo Python servers, not a system port.
# ---------------------------------------------------------------------------
Space
Sleep 300ms
Down
Space
Sleep 500ms

# ---------------------------------------------------------------------------
# Start a real Cloudflare quick tunnel, then open its QR code.
# Wait+Screen keeps the timing correct on slow networks.
# ---------------------------------------------------------------------------
Type "t"
Wait+Screen@30s /tunnel :[0-9]+ active/
Sleep 700ms
Type "q"
Wait+Screen@10s /tunnel qr :[0-9]+/
Sleep 3s

# ---------------------------------------------------------------------------
# Hidden cleanup: quit zooport and stop the demo servers.
# ---------------------------------------------------------------------------
Hide
Ctrl+C
Sleep 500ms
Type "kill %1 %2 %3 2>/dev/null"
Enter
Sleep 300ms
