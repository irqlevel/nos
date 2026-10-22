/*
 * Yandex Metrica's counter code, as Metrica generates it for this counter:
 * the tag loaded asynchronously, then the counter started with Session Replay,
 * the click map and link tracking. Its init counts the page it starts on;
 * MetrikaHits counts the pages reached without a reload after that. The id is
 * digits only -- next.config.ts refuses anything else -- so it goes into the
 * script as it is.
 */
export function metrikaScript(id: string): string {
    return (
        "(function(m,e,t,r,i,k,a){" +
        "m[i]=m[i]||function(){(m[i].a=m[i].a||[]).push(arguments)};" +
        "m[i].l=1*new Date();" +
        "for(var j=0;j<document.scripts.length;j++){if(document.scripts[j].src===r){return;}}" +
        "k=e.createElement(t),a=e.getElementsByTagName(t)[0],k.async=1,k.src=r,a.parentNode.insertBefore(k,a)" +
        `})(window,document,'script','https://mc.yandex.ru/metrika/tag.js?id=${id}','ym');` +
        `ym(${id},'init',{ssr:true,webvisor:true,clickmap:true,ecommerce:"dataLayer",` +
        "referrer:document.referrer,url:location.href,accurateTrackBounce:true,trackLinks:true});"
    );
}
