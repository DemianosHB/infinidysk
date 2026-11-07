import { afterEach, describe, expect, it, vi } from "vitest";
import {
  formatOutboundProxyTestResult,
  getChangedConfig,
  isGeneralSettingsUpdated,
  testOutboundProxy,
} from "./route";

describe("settings change persistence", () => {
  it.each(["general.base-url", "general.trust-proxy", "general.socks5-proxy"])(
    "detects a change to %s",
    (configKey) => {
      const config = {
        "general.base-url": "https://nzbdav.example.com",
        "general.trust-proxy": "false",
        "general.socks5-proxy": "socks5://proxy1.example:1080",
      };

      expect(
        isGeneralSettingsUpdated(config, {
          ...config,
          [configKey]:
            configKey === "general.base-url"
              ? "https://new.example.com"
              : configKey === "general.socks5-proxy"
                ? "socks5://proxy2.example:1080"
                : "true",
        }),
      ).toBe(true);
    },
  );

  it("includes repair settings in the save payload", () => {
    const config = {
      "repair.par2-enabled": "true",
      "repair.par2-preferred-over-arr": "true",
      "repair.par2-max-missing-slices": "8",
      "repair.par2-max-release-gb": "16",
      "repair.par2-max-memory-mb": "256",
      "repair.par2-max-patch-gb": "4",
      "repair.par2-fetch-concurrency": "2",
      "repair.par2-failure-cooldown-hours": "6",
      "repair.degraded-tolerance-enabled": "true",
      "repair.corruption-tracking-enabled": "true",
      "repair.degraded-max-consecutive-missing": "2",
      "repair.degraded-max-total-missing": "5",
      "repair.degraded-max-missing-byte-percent": "1.0",
    };
    const changedRepairs = {
      "repair.par2-enabled": "false",
      "repair.par2-preferred-over-arr": "false",
      "repair.par2-max-missing-slices": "12",
      "repair.par2-max-release-gb": "24",
      "repair.par2-max-memory-mb": "512",
      "repair.par2-max-patch-gb": "8",
      "repair.par2-fetch-concurrency": "4",
      "repair.par2-failure-cooldown-hours": "12",
      "repair.degraded-tolerance-enabled": "false",
      "repair.corruption-tracking-enabled": "false",
      "repair.degraded-max-consecutive-missing": "3",
      "repair.degraded-max-total-missing": "10",
      "repair.degraded-max-missing-byte-percent": "2.5",
    };

    expect(getChangedConfig(config, { ...config, ...changedRepairs })).toEqual(changedRepairs);
  });
});

describe("outbound proxy test helpers", () => {
  describe("formatOutboundProxyTestResult", () => {
    it("reports success for a connected result", () => {
      expect(formatOutboundProxyTestResult({ connected: true, reason: null })).toBe(
        "Proxy reachable and accepting SOCKS5 connections.",
      );
    });

    it("uses the server-provided reason when the proxy is not reachable", () => {
      expect(
        formatOutboundProxyTestResult({
          connected: false,
          reason: "The connection attempt timed out after 5 seconds.",
        }),
      ).toBe("The connection attempt timed out after 5 seconds.");
    });

    it("falls back to a generic message when no usable reason is provided", () => {
      expect(formatOutboundProxyTestResult({ connected: false, reason: null })).toBe(
        "Could not reach the proxy.",
      );
      expect(formatOutboundProxyTestResult({ connected: false, reason: "  " })).toBe(
        "Could not reach the proxy.",
      );
    });
  });

  describe("testOutboundProxy", () => {
    afterEach(() => {
      vi.unstubAllGlobals();
    });

    it("posts the proxy URL to the outbound proxy test endpoint", async () => {
      const fetchMock = vi.fn().mockResolvedValue({
        ok: true,
        status: 200,
        json: () => ({ connected: true, reason: null }),
      });
      vi.stubGlobal("fetch", fetchMock);

      await expect(testOutboundProxy("socks5://gluetun:1080")).resolves.toEqual({
        connected: true,
        reason: null,
      });

      expect(fetchMock).toHaveBeenCalledTimes(1);
      const [url, options] = fetchMock.mock.calls[0] as [string, RequestInit];
      expect(url).toContain("/api/test-outbound-proxy");
      expect(options.method).toBe("POST");
      expect(options.body).toBeInstanceOf(FormData);
      expect((options.body as FormData).get("proxy-url")).toBe("socks5://gluetun:1080");
    });

    it("rejects when the backend rejects the proxy URL", async () => {
      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValue({ ok: false, status: 400, json: () => ({}) }),
      );

      await expect(testOutboundProxy("not a url")).rejects.toThrow(
        "Proxy test failed with status 400",
      );
    });
  });
});
