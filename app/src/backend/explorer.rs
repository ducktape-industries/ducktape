use super::*;

/// The global-key router for the command palette: platform-Command+K
/// toggles, Escape closes an open palette; anything else is `none`.
pub fn palette_key_action(logical: String, modifiers: gpui_kit::Modifiers, open: bool) -> String {
    let is_toggle = command_held(modifiers) && logical.eq_ignore_ascii_case("k");
    if is_toggle {
        return match open {
            true => "close".into(),
            false => "open".into(),
        };
    }
    let closes_palette = open && logical == "escape";
    if closes_palette {
        return "close".into();
    }
    "none".into()
}

/// Is the command modifier down? The cheap half of the quit chord: it is read
/// off the native modifier stream. Command on a Mac, Control elsewhere:
/// arming and routing a chord must use the same platform modifier.
pub fn command_held(modifiers: gpui_kit::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// Is ⇧ down right now? A press carries no modifiers of its own, so the chat's
/// shift-click reads the arming this fills instead of the app growing a second
/// key route to learn the same fact.
pub fn shift_held(modifiers: gpui_kit::Modifiers) -> bool {
    modifiers.shift
}

/// Which command chord this press is (⌘Q / ⌘W, Ctrl off a Mac), or none. The
/// ONE answer, so the chords live in a single place rather than re-spelled at
/// each use site — macOS binds both through an app menu this app does not have,
/// so it reads them itself.
///
/// GPUI supplies the platform-resolved key name, including keyboard layout.
pub fn command_chord(logical: String, modifiers: gpui_kit::Modifiers) -> crate::CommandChord {
    if !command_held(modifiers) {
        return crate::CommandChord::Ignored;
    }
    let quit = logical.eq_ignore_ascii_case("q");
    if quit {
        return crate::CommandChord::Quit;
    }
    let close_window = logical.eq_ignore_ascii_case("w");
    if close_window {
        return crate::CommandChord::CloseWindow;
    }
    crate::CommandChord::Ignored
}

/// The transient layer currently over the console's content — the TOPMOST one
/// only — or `""` when the content itself is the frontmost thing on screen.
/// The order IS the z-order.
///
/// ONE enumeration, two readers. Escape asks it which surface to close;
/// the keyboard scroll asks it only whether anything at all sits over the pane
/// it would otherwise move. Every keyboard route that ignores this list routes
/// a key at the screen BEHIND the layer the reader is looking at.
///
/// NO PER-TAB RUNG IS LEFT, so the ladder takes no tab. The chat tab's menus,
/// drawer and rail belong to the chat view (`crates/views/chat`); the pages
/// tab's armed delete, block menu and comments card belong to the pages view
/// (`crates/views/pages`); the forge's switchers are the host's own pick lists.
/// Each guest paints its own scrim and dismisses its own layers — a key the
/// kernel contract carries no door for. What is enumerated here is what rides
/// EVERY tab: the native shell renders the palette and bell outside the tab.
//
// Inspect every shell layer together to name the topmost.
pub fn topmost_overlay(palette_open: bool, bell_open: bool, channel_create_open: bool) -> String {
    if palette_open {
        return "palette".into();
    }
    if bell_open {
        return "bell".into();
    }
    if channel_create_open {
        return "channel_create".into();
    }
    // THE CHAT, PAGES AND FILES LAYERS ARE THEIR VIEWS' OWN. Each holds the
    // keyboard inside its tab and answers Escape itself — the chat menus and
    // drawer, the pages armed delete, the comments card. The guest paints the
    // scrim, so the guest owns the exit.
    //
    // The forge's repository and branch switchers are the host's own pick
    // lists: the host dismisses their menus itself, so they hold no rung.
    String::new()
}

/// The surface Escape dismisses — the topmost transient layer, minus the one
/// rung Escape does not own: an open palette swallows the key itself. What is
/// left after the views took their own layers is the palette, the bell and the
/// create modal, all three of which ride every tab.
pub fn escape_target(
    logical: String,
    palette_open: bool,
    bell_open: bool,
    channel_create_open: bool,
) -> String {
    let not_escape = logical != "escape";
    if not_escape {
        return String::new();
    }
    let topmost = topmost_overlay(palette_open, bell_open, channel_create_open);
    // `palette_key_action` owns the palette's keys — an open palette swallows
    // Escape, so the ladder yields rather than naming a rung.
    let palette_owns_it = topmost == "palette";
    if palette_owns_it {
        return String::new();
    }
    topmost
}

pub fn canonical_endpoint(input: String) -> String {
    let configured = input.trim();
    rpc_client(configured)
        .map(|rpc| rpc.origin().to_string())
        .unwrap_or_else(|_| configured.to_string())
}

/// Is this press ⌘C? Its own question, not an arm on [`command_chord`]: the
/// route that asks it exists ONLY while a copy range is open, so ⌘C costs
/// nothing on a screen with no selection, and the quit/close chord — which is
/// armed by ⌘ alone, on every screen — keeps a vocabulary of two.
///
/// It reaches that route only when no widget captured the press (the
/// subscription is `status=ignored`), so a caret in a composer or a field with
/// its own selection keeps its own copy, exactly as it should.
pub fn is_copy_chord(logical: String, modifiers: gpui_kit::Modifiers) -> bool {
    command_held(modifiers) && logical.eq_ignore_ascii_case("c")
}
