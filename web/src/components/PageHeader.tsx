import type { ReactNode } from "react";

export function PageHeader({
    eyebrow,
    title,
    description,
    children,
}: {
    eyebrow?: ReactNode;
    title: string;
    description?: string;
    children?: ReactNode;
}) {
    return (
        <header className="border-b border-line bg-bg-soft/40">
            <div className="mx-auto max-w-7xl px-4 pb-10 pt-12 sm:px-6 sm:pt-16">
                {eyebrow && <div className="font-mono text-xs font-medium uppercase tracking-wider text-accent">{eyebrow}</div>}
                <h1 className="mt-3 max-w-3xl text-3xl font-bold leading-tight tracking-tight text-fg sm:text-[2.6rem]">{title}</h1>
                {description && <p className="mt-4 max-w-2xl text-lg leading-relaxed text-muted">{description}</p>}
                {children}
            </div>
        </header>
    );
}
