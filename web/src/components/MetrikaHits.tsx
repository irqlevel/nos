"use client";

import { usePathname } from "next/navigation";
import { useEffect, useRef } from "react";

declare global {
    interface Window {
        ym?: (id: number, method: string, ...args: unknown[]) => void;
    }
}

/*
 * The site moves between pages without reloading them, and Metrica's tag sees
 * no such move by itself: each one is reported as a hit, with the page left as
 * its referrer. The page the visit starts on is not -- the counter's init
 * already counted it.
 */
export function MetrikaHits({ id }: { id: string }) {
    const pathname = usePathname();
    const previous = useRef<string | null>(null);
    useEffect(() => {
        const url = window.location.href;
        if (previous.current !== null && previous.current !== url) {
            window.ym?.(Number(id), "hit", url, { referer: previous.current });
        }
        previous.current = url;
    }, [pathname, id]);
    return null;
}
