"use client";

import { useEffect } from "react";

/*
 * A copy button on every code block inside the element with this id. The
 * blocks are static HTML from the build, so the buttons are added after
 * hydration rather than rendered: React never owns that markup.
 */
export function CodeCopy({ containerId, label, done }: { containerId: string; label: string; done: string }) {
    useEffect(() => {
        const container = document.getElementById(containerId);
        if (!container) {
            return;
        }
        const added: HTMLButtonElement[] = [];
        for (const pre of container.querySelectorAll("pre")) {
            const button = document.createElement("button");
            button.type = "button";
            button.className = "copy-btn";
            button.textContent = label;
            button.addEventListener("click", async () => {
                try {
                    await navigator.clipboard.writeText(pre.querySelector("code")?.innerText ?? pre.innerText);
                    button.textContent = done;
                    button.dataset.copied = "";
                    setTimeout(() => {
                        button.textContent = label;
                        delete button.dataset.copied;
                    }, 1500);
                } catch {
                    // No clipboard (an insecure context): the text is still there to select.
                }
            });
            pre.appendChild(button);
            added.push(button);
        }
        return () => added.forEach((button) => button.remove());
    }, [containerId, label, done]);
    return null;
}
