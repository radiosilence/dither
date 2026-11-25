// An invite travels in the link's fragment, which browsers never send, so no
// server or proxy log holds it. This moves it into the join form and takes it
// off the address bar, and shows whose account it is.
const params = new URLSearchParams(location.hash.slice(1));
const invite = params.get("invite");
const field = document.querySelector<HTMLInputElement>("input[name=invite]");
if (invite && field) {
  field.value = invite;
  history.replaceState(null, "", location.pathname);
}
const username = params.get("username");
const shown = document.querySelector<HTMLInputElement>("input[name=username]");
if (username && shown) shown.value = username;
