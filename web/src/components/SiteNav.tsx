"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useState } from "react";
import { CloseIcon, MenuIcon } from "./icons";

export interface NavItem {
    href: string;
    label: string;
}

function useActive() {
    const pathname = usePathname() || "/";
    return (href: string) => pathname === href || pathname.startsWith(`${href}/`);
}

export function DesktopNav({ items, label }: { items: NavItem[]; label: string }) {
    const isActive = useActive();
    return (
        <nav aria-label={label} className="hidden items-center gap-1 md:flex">
            {items.map((item) => (
                <Link
                    key={item.href}
                    href={item.href}
                    aria-current={isActive(item.href) ? "page" : undefined}
                    className="rounded-lg px-2.5 py-1.5 text-[0.92rem] text-muted transition-colors hover:text-fg aria-[current=page]:text-fg aria-[current=page]:font-medium"
                >
                    {item.label}
                </Link>
            ))}
        </nav>
    );
}

export function MobileNav({ items, label }: { items: NavItem[]; label: string }) {
    const isActive = useActive();
    const [open, setOpen] = useState(false);
    return (
        <div className="md:hidden">
            <button
                type="button"
                aria-expanded={open}
                aria-controls="mobile-nav"
                aria-label={label}
                onClick={() => setOpen(!open)}
                className="inline-flex size-8 items-center justify-center rounded-lg text-muted hover:bg-bg-soft hover:text-fg"
            >
                {open ? <CloseIcon /> : <MenuIcon />}
            </button>
            {open && (
                <nav id="mobile-nav" aria-label={label} className="absolute inset-x-0 top-14 border-b border-line bg-bg px-4 pb-4 shadow-lg">
                    <ul className="flex flex-col">
                        {items.map((item) => (
                            <li key={item.href}>
                                <Link
                                    href={item.href}
                                    onClick={() => setOpen(false)}
                                    aria-current={isActive(item.href) ? "page" : undefined}
                                    className="block border-b border-line py-3 text-fg-soft aria-[current=page]:font-semibold aria-[current=page]:text-fg"
                                >
                                    {item.label}
                                </Link>
                            </li>
                        ))}
                    </ul>
                </nav>
            )}
        </div>
    );
}
