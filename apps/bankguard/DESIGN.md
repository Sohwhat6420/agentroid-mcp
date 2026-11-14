# Agentroid Companion: the home-screen widgets

Four widgets that replace the Nothing-style widgets on the owner's first home-screen page. They do this without
the cost: on the S24 on 14 Nov 2026, seven clock widgets from a widget app caused 3,713 of about 5,400 wakeup
alarms in one day.

## Point of view
Nothing OS's dot-matrix language, kept quiet and exact:
- dark stadium surfaces, white dot type, and one red accent for the date;
- no gradients, no glass, no shadows;
- the widget is a piece of hardware on the wallpaper, not a card.

## References (production, and what each one gives)
- **The owner's own S24 page** (a screenshot, kept privately). This is the one to match,
  and it wins over every other reference below:
  - the greeting clock (HELLO! / 06:47 / SAT, 14 NOV), white on the wallpaper with no surface;
  - the dock: six colour icons as circles on a `#2C2C2E` pill, 60 dp tall with 36 dp icons;
  - the type: a classic 5x7 LED grid, proportional. A glyph's advance is its ink width plus one cell, and a space
    is two cells. Each glyph was read dot by dot from the screenshot: 0, 1, 4, 6, 7, the colon (rows 2-3 and
    5-6), the comma (its tail one row below the baseline), and H E L O ! S A T N V.
- **Everything Widgets on the POCO** (screenshots, kept privately). Gives the row clock's proportions
  (a 2:1:1 pill, red circle and dark circle), the surface `#1B1B1D` and the red `#CD3733`, both sampled from the
  screenshot.
- **Nothing OS 2/3 widgets.** Give the dot-matrix numerals, the uppercase three-letter day, and the red dot as the
  only ornament.
- **Doto** (Óliver Lalan, SIL OFL 1.1). The open dot-matrix face nearest to Nothing's NDot.
  - Its dot is used at ROND 100 (round) and wght 900 (bold): 90 units on a 100-unit pitch, so the dots nearly touch,
    as on the owner's clock.
  - Digits, A-Z and `! , . : ?` are redrawn on that 5x7 grid with that dot; the lowercase stays Doto's own.
  - No contextual alternates. Doto's `calt` swaps the colon, and it isn't needed here.
  - Shipped as `res/font/doto_clock.ttf` (DotoClock); the licence is in `FONT-LICENSE-Doto-OFL.txt`.

## The widgets
| Widget | Size | How it stays at 0 wakeups |
| --- | --- | --- |
| Dot clock: hello (the owner's) | 3x2 | `HELLO!`, `hh:mm` at 56 sp, `EEE, d MMM` at 22 sp. No surface, no alarm |
| Dot clock: time, date, day | 4x1 | Three pills, redrawn on the system's minute tick while the screen is on; no alarm |
| Dot clock: large | 4x2 | The same, with `HH:mm` over `EEE d MMM` |

**Why the clocks are bitmaps.** A launcher inflates a widget in a restricted context, and TextView loads a resource
font only in an unrestricted one. So `android:fontFamily="@font/doto_clock"` showed the system font in a widget (seen
on the POCO, 14 Nov 2026), while it works in the app's own screens. Each clock line is drawn in the app's process
with the font (`ClockRender`) and set as an image.

**What redraws them.** GuardService, the foreground service the bank guard already runs, registers for
`ACTION_TIME_TICK`, the system's own non-wakeup minute broadcast. It redraws while the screen is on, and once at
screen-on; off, it does nothing. A clock change, timezone or locale change also redraws. The app sets no alarm, and
`updatePeriodMillis` stays 0. The service's dump counts the redraws (`clock_draws`).
| App dock | 4x1 | Up to six buttons drawn once, when the owner changes the list; no timer |

- The time follows the phone's 12 or 24-hour setting. In 12-hour it is `hh:mm`, with no am/pm, like the reference.
- **Dock, Colour (the default, as on the owner's dock):** each app's own icon as a full circle. An adaptive icon fills the
  circle; a legacy icon sits at 72% on white, the way One UI wraps one.
- **Dock, Mono:** each app's themed (monochrome) icon in white on a `#2A2A2E` circle. An app with no themed icon
  shows its own icon in greyscale.
- A bank on the bank guard's list opens from the dock through OpenSafely, so debugging is off before the bank starts.
- Tapping a clock opens the phone's alarms.

## Motion
None, on purpose. These are restrained surfaces under rule 4 of the design standard, and the battery is the point.
- Animating a widget means redrawing it on a timer, which is what this replaces.
- The showpiece libraries (GSAP, lenis, vanta) do not run in RemoteViews.

## Accessibility
- Each dock button carries its app's name as its content description, for TalkBack and for Agentroid's own taps.
- Each clock image carries its time and full date as a content description; the greeting is decorative.
- White on `#CD3733` is about 4.6:1, and on `#1B1B1D` about 16:1.
- In the config screen, rows are 56 dp and buttons at least 44 dp.

## Open
- The row and large clocks keep the POCO palette and the red date. They are extras; the greeting clock is the owner's own.
- If the system stops GuardService, the clocks keep the last minute drawn until it restarts (`START_STICKY`, the boot
  receiver, opening the app, or placing a clock).
- Confirm on the device, after 24 hours, that `dumpsys alarm` shows no wakeup alarm for the clocks. The bank guard's
  own release alarm runs only while a bank holds debugging off at screen-off.
