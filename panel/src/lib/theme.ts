export const themeStorageKey = 'theme';

/* Runs in <head> before the first paint, so a dark page is never shown
 * light first: the stored choice, else the system's -- as the site does. */
export const themeScript = `(function(){try{var s=localStorage.getItem("${themeStorageKey}");var d=s?s==="dark":matchMedia("(prefers-color-scheme: dark)").matches;var c=document.documentElement.classList;c.add(d?"dark":"light");c.remove(d?"light":"dark");}catch(e){}})();`;
