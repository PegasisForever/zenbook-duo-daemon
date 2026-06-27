/**
 * Zenbook Duo Keyboard Battery — GNOME Shell Extension
 *
 * Reads keyboard battery state from the zenbook-duo-daemon over D-Bus
 * (system bus, service "asus.zenbook.duo") and exposes it as:
 *
 *   • A panel icon + percentage label in the top-bar status area.
 *   • A Quick Settings card in the drop-down panel.
 *
 * D-Bus interface (system bus):
 *   Service   : asus.zenbook.duo
 *   Path      : /asus/zenbook/duo/State
 *   Interface : asus.zenbook.duo.State
 *
 * Properties consumed:
 *   KeyboardUsbConnected
 *   KeyboardPogoDocked
 *   BluetoothConnected
 *   KeyboardBatteryPresent
 *   KeyboardBatteryPercentage
 *   KeyboardBatteryCharging
 *   KeyboardBatteryLastKnownPresent
 *   KeyboardBatteryLastKnownPercentage
 *   KeyboardBatteryEffectiveCharging
 *   KeyboardBatteryFull
 *   DesiredPrimary
 *   DesiredSecondaryEnabled
 *   DesiredDisplayAttachment
 *   DesiredDisplayLayout
 *   DisplayBrightness
 *   DisplayApplyPaused
 *   TabletMappingEnabled
 *   TabletMappingMode
 *   TabletMappingApplyNonce
 *   SessionRegistered
 *   SessionOwner
 *   SessionLastSeenUsec
 *   MicMuteLed
 *   KeyboardBacklightLevel
 *   KeyboardBacklightEffectiveLevel
 *
 * Methods used by UI actions:
 *   SetMicMute(bool)
 *   SetKeyboardBacklightLevel(y)
 *   SetSecondaryDisplayDesired(bool)
 *   SetDesiredPrimary(string)
 *   SetTabletMappingMode(string)
 */

import GObject from 'gi://GObject';
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import St from 'gi://St';
import Clutter from 'gi://Clutter';
import Pango from 'gi://Pango';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {
    SystemIndicator,
    QuickMenuToggle,
} from 'resource:///org/gnome/shell/ui/quickSettings.js';
import {PopupAnimation} from 'resource:///org/gnome/shell/ui/boxpointer.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

// ── D-Bus constants ────────────────────────────────────────────────────────
const DBUS_SERVICE   = 'asus.zenbook.duo';
const DBUS_PATH      = '/asus/zenbook/duo/State';
const DBUS_INTERFACE = 'asus.zenbook.duo.State';

// Minimal introspection XML — only the properties we need.
const DBUS_XML = `
<node>
  <interface name="asus.zenbook.duo.State">
    <method name="ToggleMicMute"/>
    <method name="SetMicMute">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetKeyboardBacklightLevel">
      <arg name="level" type="y" direction="in"/>
    </method>
    <method name="SetSecondaryDisplayDesired">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetDesiredPrimary">
      <arg name="primary" type="s" direction="in"/>
    </method>
    <method name="SetBatteryUiConfigJson">
      <arg name="config_json" type="s" direction="in"/>
    </method>
    <method name="SetTabletMappingMode">
      <arg name="mode" type="s" direction="in"/>
    </method>
    <property name="KeyboardUsbConnected" type="b" access="read"/>
    <property name="KeyboardPogoDocked" type="b" access="read"/>
    <property name="BluetoothConnected" type="b" access="read"/>
    <property name="KeyboardConnected" type="b" access="read"/>
    <property name="KeyboardConnectionType" type="s" access="read"/>
    <property name="KeyboardBatteryPresent" type="b" access="read"/>
    <property name="KeyboardBatteryPercentage" type="y" access="read"/>
    <property name="KeyboardBatteryCharging" type="b" access="read"/>
    <property name="KeyboardBatteryLastKnownPresent" type="b" access="read"/>
    <property name="KeyboardBatteryLastKnownPercentage" type="y" access="read"/>
    <property name="KeyboardBatteryEffectiveCharging" type="b" access="read"/>
    <property name="KeyboardBatteryFull" type="b" access="read"/>
    <property name="BatteryUiConfigJson" type="s" access="read"/>
    <property name="DesiredPrimary" type="s" access="read"/>
    <property name="DesiredSecondaryEnabled" type="b" access="read"/>
    <property name="MicMuteLed" type="b" access="read"/>
    <property name="KeyboardBacklightLevel" type="y" access="read"/>
    <property name="KeyboardBacklightEffectiveLevel" type="y" access="read"/>
    <property name="DesiredDisplayAttachment" type="s" access="read"/>
    <property name="DesiredDisplayLayout" type="s" access="read"/>
    <property name="DisplayRotation" type="s" access="read"/>
    <property name="DisplayBrightness" type="u" access="read"/>
    <property name="DisplayApplyPaused" type="b" access="read"/>
    <property name="TabletMappingEnabled" type="b" access="read"/>
    <property name="TabletMappingMode" type="s" access="read"/>
    <property name="TabletMappingApplyNonce" type="u" access="read"/>
    <property name="SessionRegistered" type="b" access="read"/>
    <property name="SessionOwner" type="s" access="read"/>
    <property name="SessionLastSeenUsec" type="t" access="read"/>
    <property name="SessionQuiet" type="b" access="read"/>
  </interface>
</node>`;

const BACKLIGHT_LABELS = ['Off', 'Low', 'Medium', 'High'];
const BACKLIGHT_ICONS = [
    'keyboard-brightness-off-symbolic',
    'keyboard-brightness-medium-symbolic',
    'keyboard-brightness-medium-symbolic',
    'keyboard-brightness-high-symbolic',
];
const ZENBOOK_TILE_TITLE = 'Zen Duo';
const DEFAULT_BATTERY_UI_CONFIG = {
    discharge_warning_pct: 25,
    discharge_severe_pct: 10,
    discharge_critical_pct: 5,
    charge_half_pct: 50,
    charge_high_pct: 75,
    charge_full_pct: 100,
    discharge_warning_color: '#f9f06b',
    discharge_severe_color: '#ffbe6f',
    discharge_critical_color: '#f66151',
    charge_half_color: '#ffa348',
    charge_high_color: '#f9f06b',
    charge_full_color: '#8ff0a4',
};
function parseBatteryUiConfig(jsonText) {
    if (!jsonText)
        return {...DEFAULT_BATTERY_UI_CONFIG};

    try {
        const parsed = JSON.parse(jsonText);
        return {...DEFAULT_BATTERY_UI_CONFIG, ...parsed};
    } catch (_error) {
        return {...DEFAULT_BATTERY_UI_CONFIG};
    }
}

function batteryDisplayColor(config, present, pct, powered) {
    if (!present)
        return null;

    if (powered) {
        if (pct >= config.charge_full_pct)
            return config.charge_full_color;
        if (pct >= config.charge_high_pct)
            return config.charge_high_color;
        if (pct >= config.charge_half_pct)
            return config.charge_half_color;
        return null;
    }

    if (pct <= config.discharge_critical_pct)
        return config.discharge_critical_color;
    if (pct <= config.discharge_severe_pct)
        return config.discharge_severe_color;
    if (pct <= config.discharge_warning_pct)
        return config.discharge_warning_color;
    return null;
}

function setActorColor(actor, color) {
    actor.style = color ? `color: ${color};` : '';
}

function makeInlineStatusLabels(styleClass) {
    const labelStyleClass = styleClass === 'subtitle'
        ? 'subtitle zenbook-drawer-subtitle'
        : styleClass;
    const box = new St.BoxLayout({
        style_class: 'zenbook-inline-status',
        x_expand: true,
        x_align: Clutter.ActorAlign.FILL,
    });
    const prefix = new St.Label({
        style_class: labelStyleClass,
        y_align: Clutter.ActorAlign.CENTER,
        x_align: Clutter.ActorAlign.START,
    });
    const accent = new St.Label({
        style_class: labelStyleClass,
        y_align: Clutter.ActorAlign.CENTER,
        x_align: Clutter.ActorAlign.START,
    });
    const suffix = new St.Label({
        style_class: labelStyleClass,
        y_align: Clutter.ActorAlign.CENTER,
        x_align: Clutter.ActorAlign.START,
    });
    box.add_child(prefix);
    box.add_child(accent);
    box.add_child(suffix);
    return {box, prefix, accent, suffix};
}

function applyInlineStatus(labels, parts, color, dimmed) {
    labels.prefix.text = parts.prefix ?? '';
    labels.accent.text = parts.accent ?? '';
    labels.suffix.text = parts.suffix ?? '';
    labels.accent.visible = Boolean(parts.accent);
    labels.suffix.visible = Boolean(parts.suffix);
    setActorColor(labels.prefix, null);
    setActorColor(labels.accent, color);
    setActorColor(labels.suffix, null);
    setDimmed(labels.box, dimmed);
}

function formatBatterySuffixText(batteryPct, flowIndicator) {
    return `( ${batteryPct}%${flowIndicator ? ` ${flowIndicator}` : ''} )`;
}

function updateHealthStyle(actor, status) {
    const classes = ['zenbook-status-on', 'zenbook-status-off', 'zenbook-status-warn'];
    for (const klass of classes)
        actor.remove_style_class_name(klass);
    switch (status) {
    case 'on':
        actor.add_style_class_name('zenbook-status-on');
        break;
    case 'off':
        actor.add_style_class_name('zenbook-status-off');
        break;
    case 'warn':
        actor.add_style_class_name('zenbook-status-warn');
        break;
    default:
        break;
    }
}

function setDimmed(actor, dimmed) {
    if (dimmed)
        actor.add_style_class_name('zenbook-dimmed');
    else
        actor.remove_style_class_name('zenbook-dimmed');
}

function enableStickyToggle(item) {
    item.activate = event => {
        if (item._switch?.mapped)
            item.toggle();

        if (event?.type?.() === Clutter.EventType.KEY_PRESS &&
            event.get_key_symbol() === Clutter.KEY_space)
            return;
    };
}

function connectionStateShortText(kind) {
    if (kind === 'pogo')
        return 'POGO';
    if (kind === 'usb')
        return 'USB';
    if (kind === 'bt')
        return 'BT';
    return 'Detached';
}

function connectionStateText(kind) {
    if (kind === 'pogo')
        return 'Attached via pogo';
    if (kind === 'usb')
        return 'USB connected';
    if (kind === 'bt')
        return 'Bluetooth connected';
    return 'Detached';
}

function secondaryDisplayLabel(rotation) {
    switch (rotation) {
    case 'left-up':
        return 'Left display';
    case 'right-up':
        return 'Right display';
    case 'inverted':
        return 'Top display';
    default:
        return 'Bottom display';
    }
}

function rotationStatusText(rotation) {
    switch (rotation) {
    case 'left-up':
        return 'Rotation: left up';
    case 'right-up':
        return 'Rotation: right up';
    case 'inverted':
        return 'Rotation: inverted';
    default:
        return 'Rotation: none';
    }
}

function keyboardBatteryIndicator(connectionKind, charging) {
    if (charging || connectionKind === 'usb' || connectionKind === 'pogo')
        return '⚡';
    if (connectionKind === 'bt')
        return '↓';
    return '';
}

function formatDrawerBatteryText(connectionShort, batteryPct, flowIndicator) {
    if (batteryPct === null)
        return connectionShort;
    return `${connectionShort} ( ${batteryPct}%${flowIndicator ? ` ${flowIndicator}` : ''} )`;
}

function drawerBatteryParts(connectionShort, batteryPct, flowIndicator) {
    if (batteryPct === null)
        return {prefix: connectionShort};
    return {
        prefix: `${connectionShort} ( `,
        accent: `${batteryPct}%${flowIndicator ? ` ${flowIndicator}` : ''}`,
        suffix: ' )',
    };
}

function formatStatusBatteryText(connectionShort, batteryPct, hasLink, flowIndicator) {
    if (batteryPct === null)
        return hasLink ? `Keyboard: ${connectionShort}` : 'Keyboard: Detached ( last n/a )';
    if (!hasLink)
        return `Keyboard: Detached ( last ${batteryPct}% )`;
    return `Keyboard: ${connectionShort} ${formatBatterySuffixText(batteryPct, flowIndicator)}`;
}

function statusBatteryParts(connectionShort, batteryPct, hasLink, flowIndicator) {
    if (batteryPct === null) {
        return {
            prefix: hasLink ? `Keyboard: ${connectionShort}` : 'Keyboard: Detached ( last n/a )',
        };
    }
    if (!hasLink) {
        return {
            prefix: 'Keyboard: Detached ( last ',
            accent: `${batteryPct}%`,
            suffix: ' )',
        };
    }
    return {
        prefix: `Keyboard: ${connectionShort} ( `,
        accent: `${batteryPct}%${flowIndicator ? ` ${flowIndicator}` : ''}`,
        suffix: ' )',
    };
}

function formatSessionAge(ageSeconds) {
    if (ageSeconds < 60)
        return `${ageSeconds}s`;

    const minutes = Math.floor(ageSeconds / 60);
    const seconds = ageSeconds % 60;
    return seconds === 0 ? `${minutes}m` : `${minutes}m ${seconds}s`;
}

function variantValue(variant, fallback) {
    return variant ? variant.deepUnpack() : fallback;
}

function numberFromVariantValue(variant) {
    if (!variant)
        return null;
    const value = variant.deepUnpack();
    // Keep bigints as-is to avoid precision loss; return other types as-is
    return value;
}

// ── Quick Settings card ────────────────────────────────────────────────────

const ZenbookKeyboardBatteryToggle = GObject.registerClass(
class ZenbookKeyboardBatteryToggle extends QuickMenuToggle {
    _init(extension) {
        const duoIcon = new Gio.FileIcon({
            file: Gio.File.new_for_path(`${extension.path}/duo-display-symbolic.svg`),
        });
        super._init({
            title: ZENBOOK_TILE_TITLE,
            gicon: duoIcon,
            toggleMode: true,
        });
        this._zenbookDuoMarker = true;
        this._extension = extension;
        this._duoIcon = duoIcon;
        this.menu.setHeader(this._duoIcon, 'Zenbook Duo');
        const container = this.get_first_child();
        this._contentsToggle = container ? container.get_first_child() : null;
        this._subtitleLabel = this._contentsToggle?._subtitle ?? null;
        this._subtitleStatus = null;
        const subtitleParent = this._subtitleLabel?.get_parent?.();
        if (subtitleParent) {
            const children = subtitleParent.get_children?.() ?? [];
            const index = children.indexOf(this._subtitleLabel);
            this._subtitleStatus = makeInlineStatusLabels('subtitle');
            subtitleParent.remove_child(this._subtitleLabel);
            subtitleParent.insert_child_at_index(this._subtitleStatus.box, index >= 0 ? index : children.length);
        }

        this._healthItem = new PopupMenu.PopupImageMenuItem('Root unavailable', 'dialog-error-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._suppressSecondaryDisplayToggle = false;
        this._secondaryDisplayItem = new PopupMenu.PopupSwitchMenuItem('Secondary display', false);
        this._secondaryDisplayItem.connectObject('toggled', (_item, state) => {
            if (this._suppressSecondaryDisplayToggle)
                return;
            if (!this._extension.state?.rootReachable)
                return;
            if (this._extension.state.keyboardPogoDocked) {
                this._suppressSecondaryDisplayToggle = true;
                this._secondaryDisplayItem.setToggleState(this._extension.state.desiredSecondaryEnabled);
                this._suppressSecondaryDisplayToggle = false;
                return;
            }
            this._extension.callMethod(
                'SetSecondaryDisplayDesired',
                GLib.Variant.new('(b)', [state]),
            );
        }, this);
        enableStickyToggle(this._secondaryDisplayItem);
        this._suppressPrimaryToggle = false;
        this._rotationItem = new PopupMenu.PopupImageMenuItem('Rotation: none', 'object-rotate-right-symbolic', {
            reactive: false,
            can_focus: false,
        });
        this._primaryTopItem = new PopupMenu.PopupSwitchMenuItem('Top display as primary', false);
        this._primaryTopItem.connectObject('toggled', (_item, state) => {
            if (this._suppressPrimaryToggle)
                return;
            this._extension.callMethod(
                'SetDesiredPrimary',
                GLib.Variant.new('(s)', [state ? 'eDP-1' : 'eDP-2']),
            );
        }, this);
        enableStickyToggle(this._primaryTopItem);
        this._suppressMatchPanelsToggle = false;
        this._matchPanelsItem = new PopupMenu.PopupSwitchMenuItem('Stylus match displays', false);
        this._matchPanelsItem.connectObject('toggled', (_item, state) => {
            if (this._suppressMatchPanelsToggle)
                return;
            this._extension.callMethod(
                'SetTabletMappingMode',
                GLib.Variant.new('(s)', [state ? 'one_to_one' : 'all_to_primary']),
            );
        }, this);
        enableStickyToggle(this._matchPanelsItem);
        this._suppressMicToggle = false;
        this._micItem = new PopupMenu.PopupSwitchMenuItem('Microphone', false);
        this._micItem.connectObject('toggled', (_item, state) => {
            if (this._suppressMicToggle)
                return;
            this._extension.callMethod(
                'SetMicMute',
                GLib.Variant.new('(b)', [!state]),
            );
        }, this);
        enableStickyToggle(this._micItem);
        this._sendBacklightLevel = level => {
            this._extension.callMethod(
                'SetKeyboardBacklightLevel',
                GLib.Variant.new('(y)', [level]),
            );
        };
        this._backlightRowLabel = new St.Label({
            text: 'Backlight',
            style_class: 'zenbook-backlight-row-label',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._backlightRowIcon = new St.Icon({
            icon_name: 'input-keyboard-symbolic',
            style_class: 'popup-menu-icon',
            y_align: Clutter.ActorAlign.CENTER,
        });
        const backlightBox = new St.BoxLayout({
            style_class: 'zenbook-backlight-box',
            vertical: true,
            x_expand: true,
            x_align: Clutter.ActorAlign.FILL,
        });
        const backlightHeaderBox = new St.BoxLayout({
            style_class: 'zenbook-backlight-row',
            x_expand: true,
            x_align: Clutter.ActorAlign.FILL,
        });
        this._backlightHeaderStatus = makeInlineStatusLabels('zenbook-backlight-row-label');
        backlightHeaderBox.add_child(this._backlightRowIcon);
        backlightHeaderBox.add_child(this._backlightHeaderStatus.box);
        this._backlightTickButtons = new Map();
        const tickBox = new St.BoxLayout({
            style_class: 'zenbook-backlight-ticks',
            x_expand: true,
            x_align: Clutter.ActorAlign.FILL,
        });
        for (let level = 0; level <= 3; level++) {
            const icon = new St.Icon({
                icon_name: BACKLIGHT_ICONS[level],
                style_class: 'zenbook-backlight-level-icon',
            });
            const label = new St.Label({
                text: BACKLIGHT_LABELS[level],
                style_class: 'zenbook-backlight-level-label',
                x_align: Clutter.ActorAlign.CENTER,
            });
            const buttonContent = new St.BoxLayout({
                style_class: 'zenbook-backlight-level-content',
                vertical: true,
                x_align: Clutter.ActorAlign.CENTER,
                y_align: Clutter.ActorAlign.CENTER,
            });
            buttonContent.add_child(icon);
            buttonContent.add_child(label);
            const button = new St.Button({
                style_class: 'icon-button zenbook-backlight-level-button',
                x_expand: true,
                can_focus: true,
            });
            button.set_child(buttonContent);
            button.connectObject('clicked', () => this._sendBacklightLevel(level), this);
            const levelBox = new St.BoxLayout({
                style_class: 'zenbook-backlight-level',
                x_expand: true,
            });
            levelBox.add_child(button);
            tickBox.add_child(levelBox);
            this._backlightTickButtons.set(level, button);
        }
        backlightBox.add_child(backlightHeaderBox);
        backlightBox.add_child(tickBox);
        this._backlightButtonsItem = new PopupMenu.PopupBaseMenuItem({reactive: false});
        this._backlightButtonsItem.add_child(backlightBox);

        this.menu.addMenuItem(this._healthItem);
        this.menu.addMenuItem(this._rotationItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addMenuItem(this._secondaryDisplayItem);
        this.menu.addMenuItem(this._primaryTopItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addMenuItem(this._matchPanelsItem);
        this.menu.addMenuItem(this._backlightButtonsItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addMenuItem(this._micItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const settingsItem = new PopupMenu.PopupMenuItem('More settings...');
        settingsItem.connectObject('activate', () => {
            this._extension.openPreferences();
            this.menu.close(PopupAnimation.FADE);
        }, this);
        settingsItem.visible = Main.sessionMode.allowSettings;
        this.menu.addMenuItem(settingsItem);
        this.menu._settingsActions ??= {};
        this.menu._settingsActions[this._extension.uuid] = settingsItem;
    }

    /**
     * Update the card contents from the latest daemon state.
     *
     * @param {boolean} present
     * @param {number}  pct       0-100
     * @param {boolean} charging
     * @param {boolean} full
     */
    update(state) {
        const connectedShort = connectionStateShortText(state.keyboardConnectionType);
        const hasLink = state.keyboardConnected;
        const rotation = state.displayRotation ?? 'normal';
        const showLiveBattery = state.keyboardBatteryPresent;
        const batteryUiConfig = state.batteryUiConfig ?? DEFAULT_BATTERY_UI_CONFIG;
        const batteryPct = showLiveBattery
            ? state.keyboardBatteryPercentage
            : state.keyboardBatteryLastKnownPresent
                ? state.keyboardBatteryLastKnownPercentage
                : null;
        const charging = state.keyboardBatteryEffectiveCharging;
        const powered = charging ||
            state.keyboardConnectionType === 'usb' ||
            state.keyboardConnectionType === 'pogo';
        const flowIndicator = keyboardBatteryIndicator(state.keyboardConnectionType, charging);
        const batteryColor = batteryDisplayColor(
            batteryUiConfig,
            batteryPct !== null,
            batteryPct ?? 0,
            powered,
        );
        const subtitleDimmed = !showLiveBattery && batteryPct !== null && !hasLink;
        const buttonSubtitle = formatDrawerBatteryText(connectedShort, batteryPct, flowIndicator);
        const buttonSubtitleParts = drawerBatteryParts(connectedShort, batteryPct, flowIndicator);
        const keyboardStatusText = formatStatusBatteryText(connectedShort, batteryPct, hasLink, flowIndicator);
        const keyboardStatusParts = statusBatteryParts(connectedShort, batteryPct, hasLink, flowIndicator);
        // The daemon provides sessionQuiet to indicate stale sessions; no need to calculate age locally
        const staleSession = state.sessionQuiet === true;
        const tabletKnown = state.tabletMappingMode !== null;
        this.subtitle = buttonSubtitle;
        if (this._subtitleStatus) {
            applyInlineStatus(this._subtitleStatus, buttonSubtitleParts, batteryColor, subtitleDimmed);
        } else if (this._subtitleLabel) {
            this._subtitleLabel.text = buttonSubtitle;
            setActorColor(this._subtitleLabel, null);
            setDimmed(this._subtitleLabel, subtitleDimmed);
        }
        applyInlineStatus(this._backlightHeaderStatus, keyboardStatusParts, batteryColor, subtitleDimmed);
        if (!state.rootReachable) {
            this._healthItem.label.text = 'Root off · Session unknown';
            updateHealthStyle(this._healthItem.label, 'off');
        } else if (state.sessionRegistered === null) {
            this._healthItem.label.text = 'Root on · Session pending';
            updateHealthStyle(this._healthItem.label, 'warn');
        } else if (state.sessionRegistered) {
            this._healthItem.label.text = staleSession
                ? 'Root on · Session quiet'
                : 'Root on · Session linked';
            updateHealthStyle(this._healthItem.label, staleSession ? 'warn' : 'on');
        } else {
            this._healthItem.label.text = 'Root on · Session off';
            updateHealthStyle(this._healthItem.label, 'off');
        }
        this.gicon = this._duoIcon;
        this.checked = state.rootReachable && state.sessionRegistered === true;
        this._secondaryDisplayItem.setSensitive(state.rootReachable);
        this._secondaryDisplayItem.label.text = secondaryDisplayLabel(rotation);
        this._rotationItem.label.text = rotationStatusText(rotation);
        this._suppressSecondaryDisplayToggle = true;
        this._secondaryDisplayItem.setToggleState(state.desiredSecondaryEnabled);
        this._suppressSecondaryDisplayToggle = false;
        setDimmed(this._secondaryDisplayItem, state.keyboardPogoDocked);
        this._primaryTopItem.setSensitive(state.rootReachable);
        this._suppressPrimaryToggle = true;
        this._primaryTopItem.setToggleState(state.desiredPrimary === 'eDP-1');
        this._suppressPrimaryToggle = false;
        const backlightKnown = state.keyboardBacklightLevel !== null;
        const effectiveBacklightKnown = state.keyboardBacklightEffectiveLevel !== null;
        const backlightAvailable = state.rootReachable && hasLink && backlightKnown;
        setDimmed(this._backlightButtonsItem, !backlightAvailable);
        for (const [level, button] of this._backlightTickButtons.entries()) {
            button.reactive = backlightAvailable;
            button.can_focus = backlightAvailable;
            if (backlightKnown && level === state.keyboardBacklightLevel)
                button.add_style_class_name('zenbook-backlight-level-button-active');
            else
                button.remove_style_class_name('zenbook-backlight-level-button-active');
            if (effectiveBacklightKnown &&
                level === state.keyboardBacklightEffectiveLevel &&
                state.keyboardBacklightEffectiveLevel !== state.keyboardBacklightLevel)
                button.add_style_class_name('zenbook-backlight-level-button-effective');
            else
                button.remove_style_class_name('zenbook-backlight-level-button-effective');
        }
        if (state.micMuteLed === null) {
            this._micItem.setSensitive(false);
        } else {
            this._micItem.setSensitive(true);
            this._suppressMicToggle = true;
            this._micItem.setToggleState(!state.micMuteLed);
            this._suppressMicToggle = false;
        }

        this._matchPanelsItem.setSensitive(tabletKnown);
        if (tabletKnown) {
            this._suppressMatchPanelsToggle = true;
            this._matchPanelsItem.setToggleState(state.tabletMappingMode === 'one_to_one');
            this._suppressMatchPanelsToggle = false;
        }
    }
});

// ── Panel indicator (top-bar icons) ───────────────────────────────────────

const ZenbookKeyboardBatteryIndicator = GObject.registerClass(
class ZenbookKeyboardBatteryIndicator extends SystemIndicator {
    _init(extension) {
        super._init();
        this._zenbookDuoMarker = true;
        // Icon in the top bar (same row as system battery, clock, etc.)
        this._panelIcon = this._addIndicator();
        this._panelIcon.icon_name = 'input-keyboard-symbolic';
        this._panelIcon.visible = false;

        // Percentage label next to the icon
        this._panelLabel = new St.Label({
            style_class: 'zenbook-kbd-battery-label',
            text: '',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this._panelLabel.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
        this._panelLabel.clutter_text.line_wrap = false;
        this.add_child(this._panelLabel);
        this._panelLabel.visible = false;
        this._panelFlowLabel = new St.Label({
            style_class: 'zenbook-kbd-battery-flow',
            text: '',
            y_align: Clutter.ActorAlign.CENTER,
        });
        this.add_child(this._panelFlowLabel);
        this._panelFlowLabel.visible = false;

        // Quick Settings card
        this._toggle = new ZenbookKeyboardBatteryToggle(extension);
        this.quickSettingsItems.push(this._toggle);
    }

    /**
     * Refresh all visual elements from daemon state.
     */
    update(state) {
        const hasLink = state.keyboardConnected;
        const batteryUiConfig = state.batteryUiConfig ?? DEFAULT_BATTERY_UI_CONFIG;
        const batteryPct = state.keyboardBatteryPresent
            ? state.keyboardBatteryPercentage
            : state.keyboardBatteryLastKnownPresent
                ? state.keyboardBatteryLastKnownPercentage
                : null;
        const charging = state.keyboardBatteryEffectiveCharging;
        const powered = charging ||
            state.keyboardConnectionType === 'usb' ||
            state.keyboardConnectionType === 'pogo';
        const show = hasLink || state.keyboardBatteryLastKnownPresent;
        const batteryColor = batteryDisplayColor(
            batteryUiConfig,
            batteryPct !== null,
            batteryPct ?? 0,
            powered,
        );
        this._panelIcon.visible = show;
        this._panelLabel.visible = show;
        this._panelFlowLabel.visible = false;

        if (batteryPct !== null) {
            this._panelIcon.icon_name = 'input-keyboard-symbolic';
            const suffix = keyboardBatteryIndicator(state.keyboardConnectionType, charging);
            const labelText = `${batteryPct}%${suffix === '⚡' ? ' ⚡' : ''}`;
            this._panelLabel.text = labelText;
            setActorColor(this._panelLabel, batteryColor);
            setActorColor(this._panelIcon, null);
            setActorColor(this._panelFlowLabel, batteryColor);
            if (suffix === '↓') {
                this._panelFlowLabel.text = ' ↓';
                this._panelFlowLabel.visible = true;
            }
        } else {
            this._panelIcon.icon_name = 'input-keyboard-symbolic';
            const labelText = hasLink ? '—' : '';
            this._panelLabel.text = labelText;
            setActorColor(this._panelLabel, null);
            setActorColor(this._panelIcon, null);
            setActorColor(this._panelFlowLabel, null);
        }

        this._toggle.update(state);
    }

    destroy() {
        this._toggle = null;
        super.destroy();
    }
});

// ── Main Extension class ───────────────────────────────────────────────────

export default class ZenbookDuoExtension extends Extension {
    enable() {
        this.state = null;
        this._removeStaleIndicators();
        this._indicator = new ZenbookKeyboardBatteryIndicator(this);
        this._micRefreshInFlight = false;

        // Insert the indicator into the Quick Settings status area.
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);

        this.state = {
            rootReachable: false,
            nowUsec: Date.now() * 1000,
            keyboardUsbConnected: false,
            keyboardPogoDocked: false,
            bluetoothConnected: false,
            keyboardConnected: false,
            keyboardConnectionType: 'detached',
            keyboardBatteryPresent: false,
            keyboardBatteryPercentage: 0,
            keyboardBatteryCharging: false,
            keyboardBatteryLastKnownPresent: false,
            keyboardBatteryLastKnownPercentage: 0,
            keyboardBatteryEffectiveCharging: false,
            keyboardBatteryFull: false,
            batteryUiConfig: {...DEFAULT_BATTERY_UI_CONFIG},
            desiredPrimary: 'eDP-1',
            desiredSecondaryEnabled: false,
            desiredDisplayAttachment: 'builtin_only',
            desiredDisplayLayout: 'joined',
            displayRotation: 'normal',
            displayBrightness: null,
            displayApplyPaused: false,
            tabletMappingEnabled: null,
            tabletMappingMode: null,
            tabletMappingApplyNonce: null,
            sessionRegistered: null,
            sessionOwner: null,
            sessionLastSeenUsec: null,
            sessionQuiet: null,
            micMuteLed: null,
            keyboardBacklightLevel: null,
        };
        this._indicator.update(this.state);

        this._connectDbus();
    }

    disable() {
        this._disconnectDbus();

        if (this._indicator) {
            this._indicator.quickSettingsItems.forEach(item => item.destroy());
            this._indicator.destroy();
            this._indicator = null;
        }

        if (this._healthTimerId) {
            GLib.Source.remove(this._healthTimerId);
            this._healthTimerId = 0;
        }
    }

    _removeStaleIndicators() {
        const quickSettings = Main.panel.statusArea.quickSettings;
        const gridChildren = quickSettings?.menu?._grid?.get_children?.() ?? [];
        for (const item of gridChildren) {
            const title = item.title ?? item._title?.text ?? item.label?.text ?? '';
            if (item._zenbookDuoMarker || title === ZENBOOK_TILE_TITLE)
                item.destroy();
        }

        const indicatorChildren = quickSettings?._indicators?.get_children?.() ?? [];
        for (const indicator of indicatorChildren) {
            const hasZenbookLabel = indicator.get_children?.().some(child =>
                child.has_style_class_name?.('zenbook-kbd-battery-label')) ?? false;
            if (indicator._zenbookDuoMarker || hasZenbookLabel)
                indicator.destroy();
        }
    }

    // ── D-Bus ────────────────────────────────────────────────────────────

    _connectDbus() {
        const iface = Gio.DBusNodeInfo.new_for_xml(DBUS_XML)
            .interfaces[0]; // parsed array; first entry is our interface

        // Build a proxy using the minimal XML introspection above.
        this._proxy = new Gio.DBusProxy({
            g_connection:      Gio.DBus.system,
            g_name:            DBUS_SERVICE,
            g_object_path:     DBUS_PATH,
            g_interface_name:  DBUS_INTERFACE,
            g_interface_info:  iface,
            g_flags:           Gio.DBusProxyFlags.NONE,
        });

        this._proxy.init_async(
            GLib.PRIORITY_DEFAULT,
            null,
            (proxy, result) => {
                try {
                    proxy.init_finish(result);
                } catch (e) {
                    console.error(`[zenbook-duo] D-Bus init failed: ${e.message}`);
                    this.state = {
                        ...this.state,
                        rootReachable: false,
                        nowUsec: Date.now() * 1000,
                    };
                    this._indicator?.update(this.state);
                    return;
                }

                // Initial read
                this._refresh();

                if (!this._healthTimerId) {
                    this._healthTimerId = GLib.timeout_add_seconds(
                        GLib.PRIORITY_DEFAULT,
                        1,
                        () => {
                            this._refresh();
                            return GLib.SOURCE_CONTINUE;
                        },
                    );
                }

                // Watch for property changes pushed by the daemon.
                this._propChangeId = this._proxy.connect(
                    'g-properties-changed',
                    () => this._refresh(),
                );
                this._nameOwnerId = this._proxy.connect(
                    'notify::g-name-owner',
                    () => this._refresh(),
                );
            },
        );
    }

    _disconnectDbus() {
        if (this._proxy) {
            if (this._propChangeId) {
                this._proxy.disconnect(this._propChangeId);
                this._propChangeId = null;
            }
            if (this._nameOwnerId) {
                this._proxy.disconnect(this._nameOwnerId);
                this._nameOwnerId = null;
            }
            this._proxy = null;
        }

        if (this._healthTimerId) {
            GLib.Source.remove(this._healthTimerId);
            this._healthTimerId = 0;
        }
    }

    callMethod(methodName, parameters) {
        if (!this._proxy)
            return;

        this._proxy.call(
            methodName,
            parameters,
            Gio.DBusCallFlags.NONE,
            -1,
            null,
            (proxy, result) => {
                try {
                    proxy.call_finish(result);
                    this._refresh();
                    if (methodName === 'SetMicMute')
                        this._refreshMicMuteLed();
                } catch (e) {
                    console.error(`[zenbook-duo] D-Bus call ${methodName} failed: ${e.message}`);
                }
            },
        );
    }

    /**
     * Read all relevant properties from the cached proxy values and push
     * them to the indicator.
     */
    _refresh() {
        // Guard against calls during or after disable()
        if (!this._indicator || !this._proxy)
            return;
        if (!this._proxy || !this._indicator)
            return;

        const get = (name) => this._proxy.get_cached_property(name);

        const usbVar = get('KeyboardUsbConnected');
        const pogoVar = get('KeyboardPogoDocked');
        const btVar = get('BluetoothConnected');
        const connectedVar = get('KeyboardConnected');
        const connectionTypeVar = get('KeyboardConnectionType');
        const presentVar = get('KeyboardBatteryPresent');
        const pctVar = get('KeyboardBatteryPercentage');
        const chargingVar = get('KeyboardBatteryCharging');
        const lastKnownPresentVar = get('KeyboardBatteryLastKnownPresent');
        const lastKnownPctVar = get('KeyboardBatteryLastKnownPercentage');
        const effectiveChargingVar = get('KeyboardBatteryEffectiveCharging');
        const fullVar = get('KeyboardBatteryFull');
        const batteryUiConfigVar = get('BatteryUiConfigJson');
        const desiredPrimaryVar = get('DesiredPrimary');
        const desiredSecondaryVar = get('DesiredSecondaryEnabled');
        const desiredAttachmentVar = get('DesiredDisplayAttachment');
        const desiredLayoutVar = get('DesiredDisplayLayout');
        const rotationVar = get('DisplayRotation');
        const brightnessVar = get('DisplayBrightness');
        const displayPausedVar = get('DisplayApplyPaused');
        const tabletMappingEnabledVar = get('TabletMappingEnabled');
        const tabletMappingModeVar = get('TabletMappingMode');
        const tabletMappingApplyNonceVar = get('TabletMappingApplyNonce');
        const sessionRegisteredVar = get('SessionRegistered');
        const sessionOwnerVar = get('SessionOwner');
        const sessionLastSeenUsecVar = get('SessionLastSeenUsec');
        const sessionQuietVar = get('SessionQuiet');
        const micMuteVar = get('MicMuteLed');
        const backlightLevelVar = get('KeyboardBacklightLevel');
        const effectiveBacklightLevelVar = get('KeyboardBacklightEffectiveLevel');
        const rootReachable = Boolean(this._proxy.get_name_owner());
        const nowUsec = Date.now() * 1000;

        this.state = {
            rootReachable,
            nowUsec,
            keyboardUsbConnected: variantValue(usbVar, false),
            keyboardPogoDocked: variantValue(pogoVar, false),
            bluetoothConnected: variantValue(btVar, false),
            keyboardConnected: variantValue(connectedVar, false),
            keyboardConnectionType: variantValue(connectionTypeVar, 'detached'),
            keyboardBatteryPresent: variantValue(presentVar, false),
            keyboardBatteryPercentage: variantValue(pctVar, 0),
            keyboardBatteryCharging: variantValue(chargingVar, false),
            keyboardBatteryLastKnownPresent: variantValue(lastKnownPresentVar, false),
            keyboardBatteryLastKnownPercentage: variantValue(lastKnownPctVar, 0),
            keyboardBatteryEffectiveCharging: variantValue(effectiveChargingVar, false),
            keyboardBatteryFull: variantValue(fullVar, false),
            batteryUiConfig: parseBatteryUiConfig(variantValue(batteryUiConfigVar, null)),
            desiredPrimary: variantValue(desiredPrimaryVar, 'eDP-1'),
            desiredSecondaryEnabled: variantValue(desiredSecondaryVar, false),
            desiredDisplayAttachment: variantValue(desiredAttachmentVar, 'builtin_only'),
            desiredDisplayLayout: variantValue(desiredLayoutVar, 'joined'),
            displayRotation: variantValue(rotationVar, 'normal'),
            displayBrightness: brightnessVar ? brightnessVar.deepUnpack() : null,
            displayApplyPaused: variantValue(displayPausedVar, false),
            tabletMappingEnabled: tabletMappingEnabledVar ? tabletMappingEnabledVar.get_boolean() : null,
            tabletMappingMode: tabletMappingModeVar ? tabletMappingModeVar.deepUnpack() : null,
            tabletMappingApplyNonce: tabletMappingApplyNonceVar
                ? tabletMappingApplyNonceVar.deepUnpack()
                : null,
            sessionRegistered: sessionRegisteredVar ? sessionRegisteredVar.get_boolean() : null,
            sessionOwner: sessionOwnerVar ? sessionOwnerVar.deepUnpack() : null,
            sessionLastSeenUsec: numberFromVariantValue(sessionLastSeenUsecVar),
            sessionQuiet: sessionQuietVar ? sessionQuietVar.get_boolean() : null,
            micMuteLed: micMuteVar ? micMuteVar.get_boolean() : null,
            keyboardBacklightLevel: backlightLevelVar ? backlightLevelVar.get_byte() : null,
            keyboardBacklightEffectiveLevel: effectiveBacklightLevelVar
                ? effectiveBacklightLevelVar.get_byte()
                : null,
        };

        this._indicator.update(this.state);
        this._refreshMicMuteLed();
    }

    _refreshMicMuteLed() {
        if (!this._proxy || !this._indicator || this._micRefreshInFlight)
            return;
        if (!this._proxy.get_name_owner())
            return;

        this._micRefreshInFlight = true;
        Gio.DBus.system.call(
            DBUS_SERVICE,
            DBUS_PATH,
            'org.freedesktop.DBus.Properties',
            'Get',
            GLib.Variant.new('(ss)', [DBUS_INTERFACE, 'MicMuteLed']),
            new GLib.VariantType('(v)'),
            Gio.DBusCallFlags.NONE,
            -1,
            null,
            (_connection, result) => {
                this._micRefreshInFlight = false;

                try {
                    const reply = Gio.DBus.system.call_finish(result);
                    const [value] = reply.deepUnpack();
                    const micMuteLed = value.deepUnpack();

                    if (!this.state || this.state.micMuteLed === micMuteLed)
                        return;

                    this.state = {
                        ...this.state,
                        micMuteLed,
                        nowUsec: Date.now() * 1000,
                    };
                    this._indicator?.update(this.state);
                } catch (e) {
                    console.error(`[zenbook-duo] MicMuteLed refresh failed: ${e.message}`);
                }
            },
        );
    }
}
