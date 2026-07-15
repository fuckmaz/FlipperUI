// GPIO types — mirror the Rust serialization of the GPIO commands in
// commands/gpio.rs. The 8 user-controllable RPC pins use the same names the
// Flipper proto enum exposes (PC0, PC1, PC3, PB2, PB3, PA4, PA6, PA7).

/** Pin direction. Backed by the `GpioPinMode` proto enum on the device. */
export type GpioMode = "input" | "output";

/** Mode observed without changing the pin. `other` means the pin is currently
 * in an alternate/analog firmware mode that the GPIO RPC API cannot describe. */
export type GpioObservedMode = GpioMode | "other";

/** Internal pull resistor configuration for an input pin. Frontend-only state —
 * firmware does not expose `GetInputPull`, so we track the last value we set. */
export type GpioPull = "no" | "up" | "down";

/** The 8 user-controllable GPIO pins exposed via RPC. */
export type GpioPinName =
  | "PC0"
  | "PC1"
  | "PC3"
  | "PB2"
  | "PB3"
  | "PA4"
  | "PA6"
  | "PA7";

/** All 8 RPC pin names as a runtime array, in proto enum order. */
export const RPC_GPIO_PINS: readonly GpioPinName[] = [
  "PC0",
  "PC1",
  "PC3",
  "PB2",
  "PB3",
  "PA4",
  "PA6",
  "PA7",
] as const;

/** Per-pin state from `gpio_snapshot()`. Output and alternate-mode pins have
 * no readable level because the firmware only permits reads in INPUT mode. */
export interface GpioPinState {
  pin: GpioPinName;
  mode: GpioObservedMode;
  value: 0 | 1 | null;
}

/** Full snapshot returned by `gpio_snapshot()`. */
export interface GpioSnapshot {
  pins: GpioPinState[];
  otg: boolean;
}

// ── Physical header layout ──────────────────────────────────────────────────
//
// The Flipper Zero exposes an 18-pin GPIO header along the top edge. Pin
// numbers run 1..18 top-to-bottom in a single column. Only the 8 entries with
// `kind: "gpio"` are controllable via the RPC commands; the rest are listed
// for context (power, ground, debug, i2c, 1wire) and the OTG pin (1) is
// switched through `gpio_set_otg`.

export type PinKind = "gpio" | "power" | "ground" | "debug" | "i2c" | "1wire";

export interface HeaderPin {
  /** Header pin number (1..18). */
  number: number;
  /** MCU / signal name as shown on the silkscreen. */
  name: string;
  /** Category — drives colour and whether the pin is controllable here. */
  kind: PinKind;
  /** Short alternate-function label (e.g. "SPI MOSI"). */
  alt: string;
  /** Free-form note shown in the info card for non-RPC pins. */
  note?: string;
}

export const HEADER_PINS: readonly HeaderPin[] = [
  { number: 1, name: "+5V", kind: "power", alt: "OTG-switchable", note: "5 V rail. Off by default — toggle via the OTG switch. Sources up to ~0.5 A from the battery." },
  { number: 2, name: "PA7", kind: "gpio", alt: "SPI MOSI" },
  { number: 3, name: "PA6", kind: "gpio", alt: "SPI MISO" },
  { number: 4, name: "PA4", kind: "gpio", alt: "SPI CS" },
  { number: 5, name: "PB3", kind: "gpio", alt: "SPI SCK" },
  { number: 6, name: "PB2", kind: "gpio", alt: "—" },
  { number: 7, name: "PC3", kind: "gpio", alt: "—" },
  { number: 8, name: "PA14", kind: "debug", alt: "SWCLK", note: "ARM Serial-Wire debug clock. Reserved for the firmware debugger." },
  { number: 9, name: "+3V3", kind: "power", alt: "always on", note: "3.3 V rail — always live. Powers expansion boards." },
  { number: 10, name: "PA13", kind: "debug", alt: "SWDIO", note: "ARM Serial-Wire debug data line. Reserved for the firmware debugger." },
  { number: 11, name: "GND", kind: "ground", alt: "—", note: "Common ground." },
  { number: 12, name: "PB6", kind: "i2c", alt: "SCL", note: "I²C clock — owned by the firmware. Not exposed via RPC." },
  { number: 13, name: "PB7", kind: "i2c", alt: "SDA", note: "I²C data — owned by the firmware. Not exposed via RPC." },
  { number: 14, name: "PC1", kind: "gpio", alt: "USART1 TX" },
  { number: 15, name: "PC0", kind: "gpio", alt: "USART1 RX" },
  { number: 16, name: "PB14", kind: "1wire", alt: "1-Wire", note: "1-Wire data line — owned by the firmware." },
  { number: 17, name: "GND", kind: "ground", alt: "—", note: "Common ground." },
  { number: 18, name: "GND", kind: "ground", alt: "—", note: "Common ground." },
] as const;
