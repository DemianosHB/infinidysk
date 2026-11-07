successfully downloaded text file (SHA: e71fd8d01bf5dd388515e5b1d1e91c12dba2267c)// @vitest-environment jsdom
/* global HTMLSelectElement */
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createElement, type Dispatch, type SetStateAction, useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ProfilesSettings } from "./profiles";

const existingProfileConfig = {
  "indexers.instances": '{"Indexers":[]}',
  "profiles.instances": JSON.stringify({
    Profiles: [
      {
        Token: "existing-token",
        Name: "Existing profile",
        IndexerNames: [],
      },
    ],
  }),
};

afterEach(cleanup);

function ProfilesHarness({
  onConfigChange,
}: {
  onConfigChange?: (config: Record<string, string>) => void;
}) {
  const [config, setConfig] = useState<Record<string, string>>(existingProfileConfig);
  const setNewConfig: Dispatch<SetStateAction<Record<string, string>>> = (update) => {
    const next = typeof update === "function" ? update(config) : update;
    onConfigChange?.(next);
    setConfig(next);
  };
  return createElement(ProfilesSettings, { config, setNewConfig });
}

describe("ProfilesSettings result ordering", () => {
  it("keeps existing profiles on Off until a quality mode is selected", async () => {
    const onConfigChange = vi.fn<(config: Record<string, string>) => void>();
    const user = userEvent.setup();
    render(createElement(ProfilesHarness, { onConfigChange }));

    const ordering = screen.getByRole<HTMLSelectElement>("combobox", { name: "Result ordering" });
    expect(ordering.value).toBe("Off");

    await user.selectOptions(ordering, "ResolutionAndSource");

    expect(onConfigChange.mock.lastCall?.[0]?.["profiles.instances"]).toContain(
      '"QualitySort":"ResolutionAndSource"',
    );
  });
});
describe("ProfilesSettings preferred audio languages", () => {
  it("writes an ordered list from the tag input and clears back to null", async () => {
    const onConfigChange = vi.fn<(config: Record<string, string>) => void>();
    const user = userEvent.setup();
    render(createElement(ProfilesHarness, { onConfigChange }));

    const input = screen.getByLabelText<HTMLInputElement>(/Preferred audio languages/);
    await user.type(input, "de,");
    await user.type(input, "en{enter}");

    expect(onConfigChange.mock.lastCall?.[0]?.["profiles.instances"]).toContain(
      '"PreferredAudioLanguages":["de","en"]',
    );

    await user.click(screen.getByRole("button", { name: "Remove de" }));
    await user.click(screen.getByRole("button", { name: "Remove en" }));

    expect(onConfigChange.mock.lastCall?.[0]?.["profiles.instances"]).toContain(
      '"PreferredAudioLanguages":null',
    );
  });
});
