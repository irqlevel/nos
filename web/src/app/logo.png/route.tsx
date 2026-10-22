import { logoImage } from "@/lib/og";

export const dynamic = "force-static";

// The Organization's logo in every page's structured data (lib/seo.ts), drawn at build time.
export function GET() {
    return logoImage();
}
