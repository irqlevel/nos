"use client";

import { themeStorageKey } from "@/lib/theme";
import { MoonIcon, SunIcon } from "./icons";

export function ThemeToggle({ label }: { label: string }) {
    function toggle() {
        const classes = document.documentElement.classList;
        const dark = !classes.contains("dark");
        classes.toggle("dark", dark);
        classes.toggle("light", !dark);
        try {
            localStorage.setItem(themeStorageKey, dark ? "dark" : "light");
        } catch {
            // Storage refused (a private window): the choice lasts this page only.
        }
    }
    return (
        <button
            type="button"
            onClick={toggle}
            aria-label={label}
            title={label}
            className="inline-flex size-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-bg-soft hover:text-fg"
        >
            <SunIcon className="hidden dark:block" />
            <MoonIcon className="block dark:hidden" />
        </button>
    );
}
