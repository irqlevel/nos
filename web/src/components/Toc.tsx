"use client";

import { useEffect, useState } from "react";
import type { TocItem } from "@/lib/markdown";

// The table of contents beside a long page, marking the section on screen.
export function Toc({ items, title }: { items: TocItem[]; title: string }) {
    const [active, setActive] = useState<string | null>(null);

    useEffect(() => {
        const headings = items
            .map((item) => document.getElementById(item.id))
            .filter((el): el is HTMLElement => el !== null);
        if (headings.length === 0) {
            return;
        }
        const observer = new IntersectionObserver(
            (entries) => {
                const visible = entries.filter((e) => e.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top);
                if (visible[0]) {
                    setActive(visible[0].target.id);
                }
            },
            { rootMargin: "-72px 0px -70% 0px" }
        );
        headings.forEach((h) => observer.observe(h));
        return () => observer.disconnect();
    }, [items]);

    if (items.length === 0) {
        return null;
    }
    return (
        <nav aria-label={title} className="text-sm">
            <p className="mb-3 font-mono text-xs font-semibold uppercase tracking-wider text-muted">{title}</p>
            <ul className="space-y-1 border-l border-line">
                {items.map((item) => (
                    <li key={item.id}>
                        <a
                            href={`#${item.id}`}
                            data-active={active === item.id ? "" : undefined}
                            className={`toc-link -ml-px block border-l border-transparent py-1 leading-snug text-muted transition-colors hover:text-fg ${
                                item.depth === 3 ? "pl-6 text-[0.8rem]" : "pl-3"
                            }`}
                        >
                            {item.text}
                        </a>
                    </li>
                ))}
            </ul>
        </nav>
    );
}
