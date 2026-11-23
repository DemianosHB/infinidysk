import { describe, expect, it } from "vitest";
import { isWatchdogSettingsUpdated } from "./watchdog";

describe("isWatchdogSettingsUpdated", () => {
  it.each(["play.stall-negative-marking-enabled", "play.stall-negative-marking-minutes"])(
    "detects a change to %s",
    (key) => {
      const config = {
        "play.stall-negative-marking-enabled": "false",
        "play.stall-negative-marking-minutes": "15",
      };
      const next = { ...config, [key]: key.endsWith("enabled") ? "true" : "30" };

      expect(isWatchdogSettingsUpdated(config, next)).toBe(true);
    },
  );

  it("returns false when nothing changed", () => {
    const config = {
      "play.stall-negative-marking-enabled": "false",
      "play.stall-negative-marking-minutes": "15",
    };

    expect(isWatchdogSettingsUpdated(config, { ...config })).toBe(false);
  });
});
