'use client';

import 'uplot/dist/uPlot.min.css';

import { useEffect, useRef } from 'react';
import uPlot from 'uplot';

/** A line chart of `series` over time (seconds), in the theme's colours. */
export function Chart({ data, series, format, locale, timeLabel, height = 160 }: {
  data: [number[], ...Array<Array<number | null>>];
  series: Array<{ label: string; color: string }>;
  format: (v: number) => string;
  locale: string;
  timeLabel: string;
  height?: number;
}) {
  const box = useRef<HTMLDivElement>(null);
  const plot = useRef<uPlot | null>(null);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const css = getComputedStyle(document.documentElement);
    /* A canvas knows no CSS variables: the theme's colour, as it is now */
    const color = (c: string) => (c.startsWith('var(') ? css.getPropertyValue(c.slice(4, -1)).trim() || '#888' : c);
    const muted = color('var(--muted)');
    const clock = new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    const line = color('var(--line)');
    const opts: uPlot.Options = {
      width: el.clientWidth,
      height,
      legend: { show: true },
      cursor: { drag: { x: false, y: false } },
      scales: { x: { time: true }, y: { range: (_u, _min, max) => [0, Math.max(max || 1, 1)] } },
      axes: [
        { stroke: muted, grid: { stroke: line, width: 1 }, ticks: { stroke: line },
          values: (_u, vals) => vals.map((v) => clock.format(new Date(v * 1000))) },
        { stroke: muted, grid: { stroke: line, width: 1 }, ticks: { stroke: line }, size: 70,
          values: (_u, vals) => vals.map((v) => format(v)) },
      ],
      series: [{ label: timeLabel, value: (_u: uPlot, v: number | null) => (v === null ? '—' : clock.format(new Date(v * 1000))) },
        ...series.map((s) => ({ label: s.label, stroke: color(s.color), width: 1.5,
        value: (_u: uPlot, v: number | null) => (v === null ? '—' : format(v)) }))],
    };
    plot.current = new uPlot(opts, data as uPlot.AlignedData, el);
    const resize = new ResizeObserver(() => plot.current?.setSize({ width: el.clientWidth, height }));
    resize.observe(el);
    return () => {
      resize.disconnect();
      plot.current?.destroy();
      plot.current = null;
    };
    /* The chart is made once; its data is set below as it changes */
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    plot.current?.setData(data as uPlot.AlignedData);
  }, [data]);

  return <div ref={box} className="w-full" />;
}
