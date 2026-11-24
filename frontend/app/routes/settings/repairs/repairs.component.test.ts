// @vitest-environment jsdom
/* global HTMLInputElement */
import { cleanup, render, screen } from "@testing-library/react";
import { createElement, useState } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { RepairsSettings } from "./repairs";

function Harness({ initial }: { initial: Record<string, string> }) {
  const [config, setConfig] = useState(initial);
  return createElement(RepairsSettings, { config, setNewConfig: setConfig });
}

afterEach(cleanup);

describe("RepairsSettings read-ahead repair controls", () => {
  it("shows the read-ahead repair toggle and window input when PAR2 repair is on", () => {
    const { container } = render(
      createElement(Harness, {
        initial: {
          "repair.enable": "true",
          "repair.par2-enabled": "true",
          "repair.readahead.enabled": "true",
          "repair.readahead.window-seconds": "45",
        },
      }),
    );

    const toggle = container.querySelector<HTMLInputElement>("#readahead-repair-enabled-checkbox");
    expect(toggle).not.toBeNull();
    expect(toggle!.checked).toBe(true);
    expect(toggle!.disabled).toBe(false);
    expect(screen.getByText("Read-ahead repair")).toBeTruthy();

    const windowInput = container.querySelector<HTMLInputElement>(
      "#readahead-window-seconds-input",
    );
    expect(windowInput).not.toBeNull();
    expect(windowInput!.value).toBe("45");
    expect(windowInput!.disabled).toBe(false);
    expect(screen.getByText("Read-ahead window (seconds)")).toBeTruthy();
  });

  it("disables the read-ahead controls when PAR2 gap repair is off", () => {
    const { container } = render(
      createElement(Harness, {
        initial: {
          "repair.enable": "true",
          "repair.par2-enabled": "false",
          "repair.readahead.enabled": "false",
          "repair.readahead.window-seconds": "45",
        },
      }),
    );

    const toggle = container.querySelector<HTMLInputElement>("#readahead-repair-enabled-checkbox");
    expect(toggle).not.toBeNull();
    expect(toggle!.disabled).toBe(true);

    const windowInput = container.querySelector<HTMLInputElement>(
      "#readahead-window-seconds-input",
    );
    expect(windowInput).not.toBeNull();
    expect(windowInput!.disabled).toBe(true);
  });
});
