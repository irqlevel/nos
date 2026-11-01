import type { Element, ElementContent, Properties } from "hast";
import { parse as parseYaml } from "yaml";
import type { Locale } from "./i18n";

/*
 * A chart is written into a post as a fenced block of YAML, ```chart, and
 * becomes HTML at build time: it takes the page's font and theme, reads on a
 * phone, and needs no script and no image file. One kind so far,
 * "before-after": each row's value before and after a change, on one scale,
 * against a reference line (a dumbbell plot).
 *
 *     ```chart
 *     type: before-after
 *     unit: "%"
 *     max: 160
 *     step: 25
 *     reference: { value: 100, label: "KVM = 100%" }
 *     series: { before: before, after: after }
 *     caption: One or two sentences under the chart.
 *     groups:
 *       - rows:
 *           - { label: "sysbench CPU, 1 thread", before: 94, after: 96 }
 *       - rows:
 *           - { label: "kernel build", after: 96 }
 *     ```
 *
 * Groups are set apart by a gap, and a group may have a "name" shown above
 * it. A row with no "before" is drawn with its "after" alone. A key the
 * renderer does not know, a value off the scale, anything missing: the build
 * fails, naming the file, as it does on a broken link.
 *
 * The plot is hidden from screen readers and the caption is not: a chart
 * illustrates a table the post shows next to it, which says it all in words.
 */

interface Row {
    label: string;
    before?: number;
    after: number;
}

interface Group {
    name?: string;
    rows: Row[];
}

interface Chart {
    unit: string;
    max: number;
    step: number;
    reference?: { value: number; label: string };
    series: { before: string; after: string };
    caption?: string;
    groups: Group[];
}

// More ticks than this would crowd the axis's labels.
const MAX_TICKS = 20;

type Obj = Record<string, unknown>;

function isObj(value: unknown): value is Obj {
    return typeof value === "object" && value !== null && !Array.isArray(value);
}

function object(value: unknown, where: string): Obj {
    if (!isObj(value)) {
        throw new Error(`${where}: must be a mapping`);
    }
    return value;
}

function onlyKeys(obj: Obj, allowed: readonly string[], where: string): void {
    for (const key of Object.keys(obj)) {
        if (!allowed.includes(key)) {
            throw new Error(`${where}: unknown key "${key}" (allowed: ${allowed.join(", ")})`);
        }
    }
}

function text(obj: Obj, key: string, where: string): string {
    const value = obj[key];
    if (typeof value !== "string" || value.trim() === "") {
        throw new Error(`${where}: "${key}" must be a non-empty string`);
    }
    return value;
}

function optionalText(obj: Obj, key: string, where: string): string | undefined {
    return obj[key] === undefined ? undefined : text(obj, key, where);
}

function number(obj: Obj, key: string, where: string, min: number, max: number): number {
    const value = obj[key];
    if (typeof value !== "number" || !Number.isFinite(value) || value < min || value > max) {
        throw new Error(`${where}: "${key}" must be a number from ${min} to ${max}`);
    }
    return value;
}

function list(obj: Obj, key: string, where: string): unknown[] {
    const value = obj[key];
    if (!Array.isArray(value) || value.length === 0) {
        throw new Error(`${where}: "${key}" must be a non-empty list`);
    }
    return value;
}

function parseChart(source: string): Chart {
    let raw: unknown;
    try {
        raw = parseYaml(source);
    } catch (e) {
        throw new Error(`not YAML: ${(e as Error).message}`);
    }
    const top = object(raw, "the block");
    onlyKeys(top, ["type", "unit", "max", "step", "reference", "series", "caption", "groups"], "the block");
    if (top.type !== "before-after") {
        throw new Error(`"type" must be "before-after"`);
    }
    if (top.unit !== undefined && typeof top.unit !== "string") {
        throw new Error(`"unit" must be a string`);
    }
    const unit = (top.unit as string | undefined) ?? "";
    const max = top.max;
    if (typeof max !== "number" || !Number.isFinite(max) || max <= 0) {
        throw new Error(`"max" must be a number above 0`);
    }
    const step = number(top, "step", "the block", max / MAX_TICKS, max);

    let reference: Chart["reference"];
    if (top.reference !== undefined) {
        const ref = object(top.reference, "reference");
        onlyKeys(ref, ["value", "label"], "reference");
        reference = { value: number(ref, "value", "reference", 0, max), label: text(ref, "label", "reference") };
    }

    const series = object(top.series, "series");
    onlyKeys(series, ["before", "after"], "series");

    const groups = list(top, "groups", "the block").map((item, g): Group => {
        const where = `group ${g + 1}`;
        const group = object(item, where);
        onlyKeys(group, ["name", "rows"], where);
        const rows = list(group, "rows", where).map((rowItem, r): Row => {
            const at = `${where}, row ${r + 1}`;
            const row = object(rowItem, at);
            onlyKeys(row, ["label", "before", "after"], at);
            return {
                label: text(row, "label", at),
                before: row.before === undefined ? undefined : number(row, "before", at, 0, max),
                after: number(row, "after", at, 0, max),
            };
        });
        return { name: optionalText(group, "name", where), rows };
    });

    return {
        unit,
        max,
        step,
        reference,
        series: { before: text(series, "before", "series"), after: text(series, "after", "series") },
        caption: optionalText(top, "caption", "the block"),
        groups,
    };
}

function el(tagName: string, className: string, properties: Properties = {}, children: ElementContent[] = []): Element {
    return { type: "element", tagName, properties: { className: className.split(" "), ...properties }, children };
}

function str(value: string): ElementContent {
    return { type: "text", value };
}

/** The chart a ```chart block describes, as the <figure> that replaces the block. */
export function renderChart(source: string, locale: Locale, file: string): Element {
    let chart: Chart;
    try {
        chart = parseChart(source);
    } catch (e) {
        throw new Error(`${file}: a chart block: ${(e as Error).message}`);
    }
    const numbers = new Intl.NumberFormat(locale, { maximumFractionDigits: 2 });
    const value = (v: number) => numbers.format(v) + chart.unit;
    // A position on the scale, as a share of the plot's width.
    const at = (v: number) => `${Number(((v / chart.max) * 100).toFixed(4))}%`;

    const scale: ElementContent[] = [];
    const ticks: ElementContent[] = [];
    for (let i = 0; i * chart.step <= chart.max * (1 + 1e-9); i++) {
        const t = i * chart.step;
        scale.push(el("span", "chart-grid", { style: `left:${at(t)}` }));
        ticks.push(el("span", "chart-tick", { style: `left:${at(t)}` }, [str(value(t))]));
    }
    if (chart.reference) {
        scale.push(el("span", "chart-ref", { style: `left:${at(chart.reference.value)}` }));
    }

    const plot: ElementContent[] = [
        el("div", "chart-scale", {}, [el("div", "chart-area", {}, scale)]),
        el("div", "chart-row chart-axis", {}, [
            el("div", "chart-label"),
            el("div", "chart-cell", {}, [el("div", "chart-area", {}, ticks)]),
        ]),
    ];
    chart.groups.forEach((group, g) => {
        if (g > 0) {
            plot.push(el("div", "chart-gap"));
        }
        if (group.name) {
            plot.push(el("div", "chart-group", {}, [str(group.name)]));
        }
        for (const row of group.rows) {
            const marks: ElementContent[] = [];
            let title = `${row.label}: ${chart.series.after} ${value(row.after)}`;
            let right = row.after;
            if (row.before !== undefined) {
                const lo = Math.min(row.before, row.after);
                const hi = Math.max(row.before, row.after);
                marks.push(el("span", "chart-link", { style: `left:${at(lo)};width:${at(hi - lo)}` }));
                marks.push(el("span", "chart-dot chart-before", { style: `left:${at(row.before)}` }));
                title = `${row.label}: ${chart.series.before} ${value(row.before)}, ${chart.series.after} ${value(row.after)}`;
                right = hi;
            }
            marks.push(el("span", "chart-dot chart-after", { style: `left:${at(row.after)}` }));
            // The value goes past the rightmost dot, so neither dot hides it.
            marks.push(el("span", "chart-value", { style: `left:${at(right)}` }, [str(value(row.after))]));
            plot.push(
                el("div", "chart-row", { title }, [
                    el("div", "chart-label", {}, [str(row.label)]),
                    el("div", "chart-cell", {}, [el("div", "chart-area", {}, marks)]),
                ])
            );
        }
    });

    const legend: ElementContent[] = [
        el("span", "chart-key", {}, [el("span", "chart-swatch chart-before"), str(chart.series.before)]),
        el("span", "chart-key", {}, [el("span", "chart-swatch chart-after"), str(chart.series.after)]),
    ];
    if (chart.reference) {
        legend.push(el("span", "chart-key", {}, [el("span", "chart-swatch chart-swatch-ref"), str(chart.reference.label)]));
    }

    const children: ElementContent[] = [
        el("div", "chart-legend", { ariaHidden: "true" }, legend),
        el("div", "chart-plot", { ariaHidden: "true" }, plot),
    ];
    if (chart.caption) {
        children.push(el("figcaption", "chart-caption", {}, [str(chart.caption)]));
    }
    return el("figure", "chart not-prose", {}, children);
}
