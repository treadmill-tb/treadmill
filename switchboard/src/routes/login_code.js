const button = document.querySelector("button");
button.onclick = () =>
  navigator.clipboard
    .writeText(document.querySelector("code").textContent)
    .then(() => (button.textContent = "Copied"));
