(() => {
  const status = document.querySelector(".copy-status");
  let statusTimer;

  const announce = (message) => {
    if (!status) return;
    status.textContent = message;
    status.classList.add("visible");
    window.clearTimeout(statusTimer);
    statusTimer = window.setTimeout(() => status.classList.remove("visible"), 1800);
  };

  const fallbackCopy = (text) => {
    const input = document.createElement("textarea");
    input.value = text;
    input.setAttribute("readonly", "");
    input.style.position = "fixed";
    input.style.opacity = "0";
    document.body.appendChild(input);
    input.select();
    const copied = document.execCommand("copy");
    input.remove();
    return copied;
  };

  document.querySelectorAll("[data-copy]").forEach((button) => {
    button.addEventListener("click", async () => {
      const source = document.getElementById(button.dataset.copy);
      if (!source) return;
      const text = source.textContent.trim();
      let copied = false;
      try {
        await navigator.clipboard.writeText(text);
        copied = true;
      } catch {
        copied = fallbackCopy(text);
      }
      if (!copied) {
        announce("Copy failed — select the command manually");
        return;
      }
      const label = button.querySelector(".copy-label");
      const previous = label ? label.textContent : "Copy";
      if (label) label.textContent = "Copied";
      button.classList.add("copied");
      announce("Command copied");
      window.setTimeout(() => {
        if (label) label.textContent = previous;
        button.classList.remove("copied");
      }, 1800);
    });
  });
})();
