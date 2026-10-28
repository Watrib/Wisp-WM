#!/usr/bin/env python3
"""X11 text-selection owner for the isolated nested selection-lock smoke."""
import sys

import gi

gi.require_version("Gdk", "3.0")
gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, GLib, Gtk

window = Gtk.Window(title="Wisp X11 selection owner")
window.set_default_size(128, 64)
window.show_all()
clipboard = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)


def own():
    # Small token for a held offer, large payload for an in-flight transfer.
    text = "wisp-test-token" if sys.argv[1] == "old-offer" else "wisp-test-block-" * (1024 * 1024)
    clipboard.set_text(text, -1)
    print("OWNED", flush=True)
    return False


GLib.timeout_add(500, own)
Gtk.main()
