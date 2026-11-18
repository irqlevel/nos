"use client";

import { Fragment, useState } from "react";

/*
 * A shell command to copy: the line in a box of its own and a button that
 * puts it on the clipboard. On a narrow screen the line wraps between its
 * words only -- never inside one, where a break after the hyphen of
 * "--wipe" would read as "-" and "-wipe" -- and the button floats in the
 * corner, so the lines under it have the whole width. What is copied is the
 * command as given, one line.
 */
export function Command({ command, copy, copied }: { command: string; copy: string; copied: string }) {
    const [done, setDone] = useState(false);
    return (
        <div className="overflow-x-auto rounded-lg border border-line bg-[var(--code-bg)] px-4 py-3">
            <button
                type="button"
                onClick={async () => {
                    try {
                        await navigator.clipboard.writeText(command);
                        setDone(true);
                        setTimeout(() => setDone(false), 1500);
                    } catch {
                        // No clipboard (an insecure context): the text is still there to select.
                    }
                }}
                className="float-right mb-1 ml-3 rounded-md border border-line bg-card px-2 py-0.5 font-sans text-[0.72rem] leading-snug text-muted transition-colors hover:text-fg"
            >
                {done ? copied : copy}
            </button>
            <code className="font-mono text-[0.8rem] leading-relaxed text-fg">
                {command.split(" ").map((word, i) => (
                    <Fragment key={i}>
                        {i > 0 && " "}
                        <span className="whitespace-nowrap">{word}</span>
                    </Fragment>
                ))}
            </code>
        </div>
    );
}
