# GPUI vibrancy settings

## Intent

Give the macOS desktop app a restrained frosted-glass look, with user control over whether the window uses vibrancy and how opaque its app-drawn surfaces are.

## Goals and constraints

- Persist a Glass appearance switch and a surface-opacity percentage with the existing desktop settings.
- Apply the choice from Settings without restarting the receiver.
- Preserve the current native GPUI `WindowBackgroundAppearance::Blurred` effect when Glass is enabled; use `Opaque` when disabled.
- Make the main window and app-owned surfaces translucent so the native material is visible behind them.
- GPUI exposes a fixed blurred material, not a blur-radius control. Do not add private AppKit access or claim the radius is adjustable; explain this in Settings.
- Keep the default readable: Glass enabled with surface opacity at 78%.

## Approaches

1. **Use GPUI's native blurred window plus translucent app surfaces (selected).** Small, supported API surface; Settings can toggle it and adjust tint/surface opacity. macOS retains control of blur radius and material.
2. **Build a custom `NSVisualEffectView`/blur layer.** Could expose more native material controls, but duplicates GPUI's platform layer and relies on unsafe/private integration for radius; reject as fragile and out of proportion.
3. **Only make GPUI surfaces translucent.** Lower effort, but provides no blur behind the window and does not meet the requested vibrancy feel; reject.

## Design

Add `glass_enabled: bool` and `surface_opacity: u8` (0–100) to `Settings`, defaulting to `true` and `78`. The Settings form edits those values and applies them with the existing Apply action. Apply updates GPUI's window background appearance and the process-wide app-surface opacity immediately; it does not restart the network service for visual-only changes.

The opacity is the alpha of app-drawn background surfaces: 0 lets the native material show through most, and 100 is opaque. Blur strength is system-managed and fixed by GPUI.

## Non-goals

- User-adjustable blur radius, material type, or custom AppKit blur implementation.
- Changing the app's color scheme or transfer behavior.
