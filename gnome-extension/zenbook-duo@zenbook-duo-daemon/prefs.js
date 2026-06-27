import Adw from 'gi://Adw';
import Gdk from 'gi://Gdk';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

const DBUS_SERVICE = 'asus.zenbook.duo';
const DBUS_PATH = '/asus/zenbook/duo/State';
const DBUS_INTERFACE = 'asus.zenbook.duo.State';

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

const IDLE_TIMEOUT_OPTIONS = [
    {seconds: 0, label: 'Off'},
    {seconds: 5, label: '5 seconds'},
    {seconds: 15, label: '15 seconds'},
    {seconds: 30, label: '30 seconds'},
    {seconds: 60, label: '1 minute'},
    {seconds: 300, label: '5 minutes'},
];

function readProperty(name) {
    const reply = Gio.DBus.system.call_sync(
        DBUS_SERVICE,
        DBUS_PATH,
        'org.freedesktop.DBus.Properties',
        'Get',
        GLib.Variant.new('(ss)', [DBUS_INTERFACE, name]),
        new GLib.VariantType('(v)'),
        Gio.DBusCallFlags.NONE,
        -1,
        null,
    );
    const [value] = reply.deepUnpack();
    return value.deepUnpack();
}

function callMethod(name, signature, values) {
    Gio.DBus.system.call_sync(
        DBUS_SERVICE,
        DBUS_PATH,
        DBUS_INTERFACE,
        name,
        GLib.Variant.new(signature, values),
        null,
        Gio.DBusCallFlags.NONE,
        -1,
        null,
    );
}

function readBatteryUiConfig() {
    try {
        return {
            ...DEFAULT_BATTERY_UI_CONFIG,
            ...JSON.parse(readProperty('BatteryUiConfigJson')),
        };
    } catch (error) {
        return {...DEFAULT_BATTERY_UI_CONFIG, _error: error.message};
    }
}

function readBehaviorSettings() {
    try {
        return {
            ambientLightAvailable: Boolean(readProperty('AmbientLightAvailable')),
            ambientKeyboardBacklightEnabled: Boolean(readProperty('AmbientKeyboardBacklightEnabled')),
            idleTimeoutSeconds: Number(readProperty('IdleTimeoutSeconds')),
        };
    } catch (error) {
        return {
            ambientLightAvailable: false,
            ambientKeyboardBacklightEnabled: false,
            idleTimeoutSeconds: 300,
            _error: error.message,
        };
    }
}

function writeBatteryUiConfig(config) {
    callMethod('SetBatteryUiConfigJson', '(s)', [JSON.stringify(config)]);
}

function setAmbientKeyboardBacklightEnabled(enabled) {
    callMethod('SetAmbientKeyboardBacklightEnabled', '(b)', [enabled]);
}

function setIdleTimeoutSeconds(seconds) {
    callMethod('SetIdleTimeoutSeconds', '(u)', [seconds]);
}

function rgbaFromHex(color) {
    const rgba = new Gdk.RGBA();
    rgba.parse(color);
    return rgba;
}

function rgbaToHex(rgba) {
    const toHex = component => Math.round(component * 255).toString(16).padStart(2, '0');
    return `#${toHex(rgba.red)}${toHex(rgba.green)}${toHex(rgba.blue)}`;
}

function buildTripletBox(children) {
    const box = new Gtk.Box({
        orientation: Gtk.Orientation.HORIZONTAL,
        spacing: 12,
        homogeneous: true,
        hexpand: true,
        halign: Gtk.Align.FILL,
    });
    for (const child of children)
        box.append(child);
    return box;
}

function buildColumnLabel(text) {
    return new Gtk.Label({
        label: text,
        halign: Gtk.Align.CENTER,
        hexpand: true,
    });
}

function buildSpin(value) {
    const spin = Gtk.SpinButton.new_with_range(0, 100, 1);
    spin.set_valign(Gtk.Align.CENTER);
    spin.set_value(value);
    spin.set_width_chars(4);
    spin.set_hexpand(true);
    spin.set_halign(Gtk.Align.CENTER);
    return spin;
}

function buildColorButton(color) {
    const colorButton = new Gtk.ColorButton({
        valign: Gtk.Align.CENTER,
        use_alpha: false,
        halign: Gtk.Align.CENTER,
    });
    colorButton.set_rgba(rgbaFromHex(color));
    return colorButton;
}

function buildTripletRow(title, widgets) {
    const row = new Adw.ActionRow({title});
    row.add_suffix(buildTripletBox(widgets));
    row.activatable = false;
    return row;
}

function isValidBatteryUiConfig(config) {
    return config.discharge_critical_pct < config.discharge_severe_pct &&
        config.discharge_severe_pct < config.discharge_warning_pct &&
        config.discharge_warning_pct <= 100 &&
        config.charge_half_pct < config.charge_high_pct &&
        config.charge_high_pct < config.charge_full_pct &&
        config.charge_full_pct <= 100;
}

export default class ZenbookDuoPreferences extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        window.set_title('Zenbook Duo Settings');
        window.set_default_size(720, 680);

        const batteryConfig = readBatteryUiConfig();
        const behaviorConfig = readBehaviorSettings();
        const page = new Adw.PreferencesPage();
        const behaviorGroup = new Adw.PreferencesGroup({
            title: 'More settings',
            description: 'Idle timeout and ambient backlight behavior live here instead of the quick drawer.',
        });
        const dischargeGroup = new Adw.PreferencesGroup({
            title: 'Discharging levels',
            description: 'Three configurable levels: Medium, Low, and Critical.',
        });
        const chargeGroup = new Adw.PreferencesGroup({
            title: 'Charging levels',
            description: 'Three configurable levels: Low, Medium, and Full.',
        });

        let syncingAmbientRow = false;
        const ambientRow = new Adw.SwitchRow({
            title: 'Low-light keyboard backlight',
            subtitle: behaviorConfig.ambientLightAvailable
                ? 'Raise the effective keyboard backlight to at least Low when the ambient sensor reports darkness.'
                : 'Ambient light sensor unavailable.',
            active: behaviorConfig.ambientKeyboardBacklightEnabled,
        });
        ambientRow.set_sensitive(behaviorConfig.ambientLightAvailable);
        ambientRow.connect('notify::active', row => {
            if (syncingAmbientRow)
                return;
            const previous = !row.get_active();
            try {
                setAmbientKeyboardBacklightEnabled(row.get_active());
            } catch (error) {
                syncingAmbientRow = true;
                row.set_active(previous);
                syncingAmbientRow = false;
                logError(error, 'Failed to save ambient keyboard backlight preference');
            }
        });

        const timeoutRow = new Adw.ActionRow({
            title: 'Keyboard backlight idle timeout',
            subtitle: 'Turning the backlight on from the drawer resets the timer from that moment.',
        });
        const timeoutModel = Gtk.StringList.new(IDLE_TIMEOUT_OPTIONS.map(option => option.label));
        const timeoutDropdown = new Gtk.DropDown({
            model: timeoutModel,
            valign: Gtk.Align.CENTER,
        });
        let syncingTimeoutDropdown = false;
        let selectedIndex = Math.max(0, IDLE_TIMEOUT_OPTIONS.findIndex(
            option => option.seconds === behaviorConfig.idleTimeoutSeconds));
        timeoutDropdown.set_selected(selectedIndex);
        timeoutDropdown.connect('notify::selected', dropdown => {
            if (syncingTimeoutDropdown)
                return;
            const previousIndex = selectedIndex;
            const option = IDLE_TIMEOUT_OPTIONS[dropdown.get_selected()];
            if (!option)
                return;
            try {
                setIdleTimeoutSeconds(option.seconds);
                selectedIndex = dropdown.get_selected();
            } catch (error) {
                syncingTimeoutDropdown = true;
                dropdown.set_selected(previousIndex);
                syncingTimeoutDropdown = false;
                logError(error, 'Failed to save keyboard idle timeout preference');
            }
        });
        timeoutRow.add_suffix(timeoutDropdown);
        timeoutRow.activatable = false;

        behaviorGroup.add(ambientRow);
        behaviorGroup.add(timeoutRow);

        let currentBatteryConfig = {...batteryConfig};
        const dischargeSpins = {
            medium: buildSpin(batteryConfig.discharge_warning_pct),
            low: buildSpin(batteryConfig.discharge_severe_pct),
            critical: buildSpin(batteryConfig.discharge_critical_pct),
        };
        const dischargeColors = {
            medium: buildColorButton(batteryConfig.discharge_warning_color),
            low: buildColorButton(batteryConfig.discharge_severe_color),
            critical: buildColorButton(batteryConfig.discharge_critical_color),
        };
        dischargeGroup.add(buildTripletRow('Levels', [
            buildColumnLabel('Medium'),
            buildColumnLabel('Low'),
            buildColumnLabel('Critical'),
        ]));
        dischargeGroup.add(buildTripletRow('Percentages', [
            dischargeSpins.medium,
            dischargeSpins.low,
            dischargeSpins.critical,
        ]));
        dischargeGroup.add(buildTripletRow('Colors', [
            dischargeColors.medium,
            dischargeColors.low,
            dischargeColors.critical,
        ]));

        const chargeSpins = {
            low: buildSpin(batteryConfig.charge_half_pct),
            medium: buildSpin(batteryConfig.charge_high_pct),
            full: buildSpin(batteryConfig.charge_full_pct),
        };
        const chargeColors = {
            low: buildColorButton(batteryConfig.charge_half_color),
            medium: buildColorButton(batteryConfig.charge_high_color),
            full: buildColorButton(batteryConfig.charge_full_color),
        };
        chargeGroup.add(buildTripletRow('Levels', [
            buildColumnLabel('Low'),
            buildColumnLabel('Medium'),
            buildColumnLabel('Full'),
        ]));
        chargeGroup.add(buildTripletRow('Percentages', [
            chargeSpins.low,
            chargeSpins.medium,
            chargeSpins.full,
        ]));
        chargeGroup.add(buildTripletRow('Colors', [
            chargeColors.low,
            chargeColors.medium,
            chargeColors.full,
        ]));
        let syncingBatteryWidgets = false;
        const readBatteryWidgets = () => ({
            discharge_warning_pct: dischargeSpins.medium.get_value_as_int(),
            discharge_severe_pct: dischargeSpins.low.get_value_as_int(),
            discharge_critical_pct: dischargeSpins.critical.get_value_as_int(),
            charge_half_pct: chargeSpins.low.get_value_as_int(),
            charge_high_pct: chargeSpins.medium.get_value_as_int(),
            charge_full_pct: chargeSpins.full.get_value_as_int(),
            discharge_warning_color: rgbaToHex(dischargeColors.medium.get_rgba()),
            discharge_severe_color: rgbaToHex(dischargeColors.low.get_rgba()),
            discharge_critical_color: rgbaToHex(dischargeColors.critical.get_rgba()),
            charge_half_color: rgbaToHex(chargeColors.low.get_rgba()),
            charge_high_color: rgbaToHex(chargeColors.medium.get_rgba()),
            charge_full_color: rgbaToHex(chargeColors.full.get_rgba()),
        });
        const writeBatteryWidgets = config => {
            syncingBatteryWidgets = true;
            dischargeSpins.medium.set_value(config.discharge_warning_pct);
            dischargeSpins.low.set_value(config.discharge_severe_pct);
            dischargeSpins.critical.set_value(config.discharge_critical_pct);
            chargeSpins.low.set_value(config.charge_half_pct);
            chargeSpins.medium.set_value(config.charge_high_pct);
            chargeSpins.full.set_value(config.charge_full_pct);
            dischargeColors.medium.set_rgba(rgbaFromHex(config.discharge_warning_color));
            dischargeColors.low.set_rgba(rgbaFromHex(config.discharge_severe_color));
            dischargeColors.critical.set_rgba(rgbaFromHex(config.discharge_critical_color));
            chargeColors.low.set_rgba(rgbaFromHex(config.charge_half_color));
            chargeColors.medium.set_rgba(rgbaFromHex(config.charge_high_color));
            chargeColors.full.set_rgba(rgbaFromHex(config.charge_full_color));
            syncingBatteryWidgets = false;
        };
        const persistBatteryConfigIfValid = () => {
            if (syncingBatteryWidgets)
                return;
            const nextConfig = readBatteryWidgets();
            if (!isValidBatteryUiConfig(nextConfig))
                return;
            try {
                writeBatteryUiConfig(nextConfig);
                currentBatteryConfig = nextConfig;
            } catch (error) {
                writeBatteryWidgets(currentBatteryConfig);
                logError(error, 'Failed to save battery UI config');
            }
        };
        [
            dischargeSpins.medium,
            dischargeSpins.low,
            dischargeSpins.critical,
            chargeSpins.low,
            chargeSpins.medium,
            chargeSpins.full,
        ].forEach(spin => spin.connect('value-changed', persistBatteryConfigIfValid));
        [
            dischargeColors.medium,
            dischargeColors.low,
            dischargeColors.critical,
            chargeColors.low,
            chargeColors.medium,
            chargeColors.full,
        ].forEach(button => button.connect('notify::rgba', persistBatteryConfigIfValid));

        page.add(behaviorGroup);
        page.add(dischargeGroup);
        page.add(chargeGroup);
        window.add(page);
    }
}
