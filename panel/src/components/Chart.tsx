'use client';

import 'uplot/dist/uPlot.min.css';

import { useEffect, useRef } from 'react';
import uPlot from 'uplot';

/** The time axis's steps, in seconds: whole minutes and up, so that a label
 * is never a time with its seconds -- the legend has those, for the point
 * the cursor is on -- and two minutes between one and five, which uPlot's
 * own steps lack: a half-width chart of a few minutes had one label. */
const TIME_STEPS = [60, 120, 300, 600, 900, 1200, 1800, 3600, 7200, 10800, 21600, 43200, 86400];

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
  /* What a chart made again starts from: the data as it was last set */
  const latest = useRef(data);

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const clock = new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    const minutes = new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit' });
    let dark: boolean | null = null;
    /* A canvas knows no CSS variables: the chart is made in the theme's
     * colours as they are, and made again when the theme changes -- a chart
     * kept from before would draw the light theme's lines on the dark. */
    const make = () => {
      const now = document.documentElement.classList.contains('dark');
      if (now === dark) return;
      dark = now;
      const css = getComputedStyle(document.documentElement);
      const color = (c: string) => (c.startsWith('var(') ? css.getPropertyValue(c.slice(4, -1)).trim() || '#888' : c);
      const muted = color('var(--muted)');
      const line = color('var(--line)');
      const opts: uPlot.Options = {
        width: el.clientWidth,
        height,
        legend: { show: true },
        cursor: { drag: { x: false, y: false } },
        /* Room on the right for the last time's label, which is centred on
         * its tick */
        padding: [null, 32, null, null],
        scales: { x: { time: true }, y: { range: (_u, _min, max) => [0, Math.max(max || 1, 1)] } },
        axes: [
          /* Ticks further apart than a time is wide: a half-width chart's
           * times, each with its seconds, ran into each other */
          { stroke: muted, grid: { stroke: line, width: 1 }, ticks: { stroke: line }, space: 70, incrs: TIME_STEPS,
            values: (_u, vals) => vals.map((v) => minutes.format(new Date(v * 1000))) },
          { stroke: muted, grid: { stroke: line, width: 1 }, ticks: { stroke: line }, size: 70,
            values: (_u, vals) => vals.map((v) => format(v)) },
        ],
        series: [{ label: timeLabel, value: (_u: uPlot, v: number | null) => (v === null ? '—' : clock.format(new Date(v * 1000))) },
          ...series.map((s) => ({ label: s.label, stroke: color(s.color), width: 1.5,
          value: (_u: uPlot, v: number | null) => (v === null ? '—' : format(v)) }))],
      };
      plot.current?.destroy();
      plot.current = new uPlot(opts, latest.current as uPlot.AlignedData, el);
      /* The theme it was drawn in, for the tests to hold it to the page's */
      el.dataset.theme = now ? 'dark' : 'light';
    };
    make();
    const resize = new ResizeObserver(() => plot.current?.setSize({ width: el.clientWidth, height }));
    resize.observe(el);
    const theme = new MutationObserver(make);
    theme.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
    return () => {
      theme.disconnect();
      resize.disconnect();
      plot.current?.destroy();
      plot.current = null;
    };
    /* The chart is made once a theme; its data is set below as it changes */
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    latest.current = data;
    plot.current?.setData(data as uPlot.AlignedData);
  }, [data]);

  return <div ref={box} className="w-full" data-testid="chart" />;
}
