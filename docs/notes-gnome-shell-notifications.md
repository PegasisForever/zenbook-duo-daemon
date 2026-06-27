# GNOME Shell / freedesktop notifications (empirical notes)

Operational notes for anyone tuning **`src/session/notifications.rs`** (zbus `Notify`).

**“Empirical” here** means behaviour was **measured on a live GNOME Shell** (`notify-send`, `gdbus` introspection), not inferred only from the freedesktop specification. GNOME is free to ignore client hints; these notes capture what **actually happened** on one stack (**GNOME 50.1**, 2026-05-13) so we do not misread `expire_timeout` as controlling banner dwell.

## D-Bus server introspection (reference machine)

**`GetServerInformation` →** `('gnome-shell', 'GNOME', '50.1', '1.2')`

**`GetCapabilities` →** `['actions', 'body', 'body-markup', 'icon-static', 'persistence', 'sound']`

There is **no** capability flag that promises “honour `expire_timeout` for on-screen banner time.” Capabilities only describe features (markup, actions, etc.).

## `expire_timeout` / `-t` (client hint)

Per the [Desktop Notifications Specification](https://specifications.freedesktop.org/notification-spec/latest/), **`expire_timeout` is a hint**; the server may ignore it.

**Observed:** for **transient** toasts with **normal** urgency, the **visible banner** lasts about **~3–3.5 s** whether `-t` is **500 ms**, **20 s**, or **30 s**. So **`-t` does not control dwell time** for that class of notification on this stack; it is **not** “broken client args,” it is **Shell policy**.

**Observed:** for **critical** urgency (`urgency` byte **2**, `-u critical`, `dialog-error`–style), the banner becomes **sticky / long-lived** (effectively until dismissed or cleared from the tray), again **independent of a short `-t`** in practice.

**Observed:** adding **`transient: true`** did **not** make critical behave like a short toast — still sticky.

**Observed:** **critical + explicit `transient:false` + `resident:true`** looked the **same** as “plain critical” to the eye — no extra lever found here on GNOME 50.1.

**Observed:** **non-critical, non-transient** (“persistent-style”): short banner (~3–5 s), then entry **recoils into the notification list** and persists there — matches “persistent” = tray, not long banner.

## `replaces_id` / replacement

**Observed:** chaining **`--replace-id`** / `replaces_id` can update the same logical notification; after the first toast was **manually dismissed**, a later replace still produced a sensible new notification (IDs on the bus do not always stay numerically identical after long gaps or expiry — server-dependent).

## Relation to daemon battery tiers (reminder)

| Tier (daemon) | Hints (summary) | `expire_timeout` in code | Banner vs tray (GNOME) |
|---------------|-----------------|---------------------------|-------------------------|
| Below 20 % | `transient` + `resident: false` + `image-path` | 30_000 ms | **Short** banner; `-t` not authoritative for dwell |
| Below 10 % | same transient family | 120_000 ms | Still **short** banner; long timeout ≠ long bubble |
| Below 5 % | **only** `urgency: 2` + `image-path` (no transient) | 120_000 ms | **Sticky** critical-style behaviour; timeout hint not the real “off switch” |

**Implication:** if we ever need **guaranteed** dismissal after N seconds (e.g. replace critical with USB-charging copy), rely on **`CloseNotification`** or a **follow-up `Notify` with `replaces_id`**, not on `expire_timeout` alone.

## Commands used (repeatable)

```bash
gdbus call --session --dest org.freedesktop.Notifications \
  --object-path /org/freedesktop/Notifications \
  --method org.freedesktop.Notifications.GetCapabilities

gdbus call --session --dest org.freedesktop.Notifications \
  --object-path /org/freedesktop/Notifications \
  --method org.freedesktop.Notifications.GetServerInformation
```

`notify-send --help` on Fedora: **`-p` / `--print-id`**, **`-r` / `--replace-id`**, **`-e` / `--transient`**, **`-t`**, **`-h`** for hints.
