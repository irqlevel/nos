import { notFound } from "next/navigation";

// Any path under a locale that no page claims: the locale's own 404, inside its layout.
export default function CatchAll() {
    notFound();
}
