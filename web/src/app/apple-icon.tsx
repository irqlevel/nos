import { ImageResponse } from "next/og";

export const size = { width: 180, height: 180 };
export const contentType = "image/png";

// The favicon's prompt, at the size iOS puts on a home screen.
export default function AppleIcon() {
    return new ImageResponse(
        (
            <div style={{ display: "flex", width: "100%", height: "100%", background: "#0d0e11", alignItems: "center", justifyContent: "center" }}>
                <svg width="132" height="132" viewBox="0 0 64 64">
                    <path d="M17 21l11 11-11 11" fill="none" stroke="#f2a93b" strokeWidth="6" strokeLinecap="round" strokeLinejoin="round" />
                    <path d="M33 44h14" stroke="#ebe9e4" strokeWidth="6" strokeLinecap="round" />
                </svg>
            </div>
        ),
        size
    );
}
