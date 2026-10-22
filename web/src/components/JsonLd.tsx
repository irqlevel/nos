// Structured data for search engines. "<" is escaped so no string in it can close the script.
export function JsonLd({ data }: { data: object }) {
    const json = JSON.stringify(data).replace(/</g, "\\u003c");
    return <script type="application/ld+json" dangerouslySetInnerHTML={{ __html: json }} />;
}
