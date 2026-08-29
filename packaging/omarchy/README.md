# Hyprcorrect for the Omarchy bar

The companion replaces Hyprcorrect's StatusNotifier tray icon while its native
bar widget is attached. If the widget, Quickshell, or its bridge exits, the
daemon makes the existing tray item active again on the next 500 ms heartbeat.

Left-click opens compact controls for pause/resume, quick and smart provider
routing, Vim review mode, and the current keybindings. Middle-click toggles
pause; right-click opens Hyprcorrect's full Preferences window.

Secrets, provider endpoints, LanguageTool installation, hotkey recording,
reset-key behavior, and per-app privacy intentionally remain in Preferences.
The QML side only receives bounded status JSON from the native bridge, renders
external strings as plain text, and invokes fixed native subcommands without
shell interpolation.
