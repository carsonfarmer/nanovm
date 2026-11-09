// Relative URLs: the site is served under its object's path (`/www/`).
const likes = document.getElementById("likes");
const show = async (res) => { likes.textContent = (await res.json()).likes; };
fetch("api/likes").then(show);
document.getElementById("like").onclick = () => fetch("api/likes", { method: "POST" }).then(show);
