import fs from "node:fs";
import path from "node:path";
import { ImageResponse } from "next/og";
import { site } from "./site";

export const ogSize = { width: 1200, height: 630 };

/*
 * The fonts the cards are set in, read from the npm packages at build time:
 * static WOFF (the renderer reads neither WOFF2 nor variable fonts), the Latin
 * and the Cyrillic subset of each, so a Russian title renders as an English one does.
 */
function font(pkg: string, file: string): Buffer {
    return fs.readFileSync(path.join(process.cwd(), "node_modules", "@fontsource", pkg, "files", file));
}

interface Font {
    name: string;
    data: Buffer;
    weight: 400 | 700;
    style: "normal";
}

let fonts: Font[] | undefined;

function loadFonts(): Font[] {
    /*
     * Each subset is a family of its own and every element names both, Latin
     * first: the renderer keeps one font per family and weight, so a second
     * subset under the same name would never be reached.
     */
    fonts ??= [
        { name: "InterBoldLatin", data: font("inter", "inter-latin-700-normal.woff"), weight: 700, style: "normal" },
        { name: "InterBoldCyrillic", data: font("inter", "inter-cyrillic-700-normal.woff"), weight: 700, style: "normal" },
        { name: "InterLatin", data: font("inter", "inter-latin-400-normal.woff"), weight: 400, style: "normal" },
        { name: "InterCyrillic", data: font("inter", "inter-cyrillic-400-normal.woff"), weight: 400, style: "normal" },
        { name: "MonoLatin", data: font("jetbrains-mono", "jetbrains-mono-latin-700-normal.woff"), weight: 700, style: "normal" },
        { name: "MonoCyrillic", data: font("jetbrains-mono", "jetbrains-mono-cyrillic-700-normal.woff"), weight: 700, style: "normal" },
    ];
    return fonts;
}

// The card every shared link shows: the wordmark, what the page is, its title.
export function ogImage({ eyebrow, title, footer }: { eyebrow: string; title: string; footer: string }) {
    const size = title.length > 70 ? 52 : title.length > 45 ? 60 : 68;
    return new ImageResponse(
        (
            <div
                style={{
                    display: "flex",
                    flexDirection: "column",
                    justifyContent: "space-between",
                    width: "100%",
                    height: "100%",
                    padding: "64px 72px",
                    background: "#0d0e11",
                    backgroundImage: "radial-gradient(#262930 1.5px, transparent 1.5px)",
                    backgroundSize: "28px 28px",
                    color: "#ebe9e4",
                    fontFamily: "InterLatin, InterCyrillic",
                }}
            >
                <div style={{ display: "flex", fontFamily: "MonoLatin, MonoCyrillic", fontSize: 46, fontWeight: 700 }}>
                    nos<span style={{ color: "#f2a93b" }}>_</span>
                </div>
                <div style={{ display: "flex", flexDirection: "column" }}>
                    <div style={{ display: "flex", fontFamily: "MonoLatin, MonoCyrillic", fontSize: 26, fontWeight: 700, color: "#f2a93b" }}>{eyebrow}</div>
                    <div style={{ display: "flex", marginTop: 20, fontFamily: "InterBoldLatin, InterBoldCyrillic", fontSize: size, fontWeight: 700, lineHeight: 1.12, letterSpacing: -1.5 }}>{title}</div>
                </div>
                <div style={{ display: "flex", justifyContent: "space-between", fontSize: 24, color: "#9a9fa9" }}>
                    <span>{footer}</span>
                    <span>github.com/irqlevel/nos</span>
                </div>
            </div>
        ),
        { ...ogSize, fonts: loadFonts() }
    );
}

// The logo: the wordmark, square, on the cards' ground -- what search engines show for the site.
export function logoImage() {
    return new ImageResponse(
        (
            <div
                style={{
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    width: "100%",
                    height: "100%",
                    background: "#0d0e11",
                    color: "#ebe9e4",
                    fontFamily: "MonoLatin",
                    fontSize: 168,
                    fontWeight: 700,
                    letterSpacing: -6,
                }}
            >
                nos<span style={{ color: "#f2a93b" }}>_</span>
            </div>
        ),
        { width: site.logo.size, height: site.logo.size, fonts: loadFonts() }
    );
}
