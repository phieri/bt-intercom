try {
  const theme = localStorage.getItem("theme");
  if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
} catch {}

document.addEventListener("DOMContentLoaded", () => {
  const themeToggle = document.getElementById("theme-toggle");
  const themeColor = document.querySelector('meta[name="theme-color"]');
  const colorScheme = window.matchMedia("(prefers-color-scheme: dark)");
  const currentTheme = () => document.documentElement.dataset.theme || (colorScheme.matches ? "dark" : "light");
  const updateThemeToggle = () => {
    const theme = currentTheme();
    const nextTheme = theme === "dark" ? "light" : "dark";
    themeToggle.textContent = `${nextTheme === "light" ? "☼" : "☾"} ${nextTheme} mode`;
    themeToggle.setAttribute("aria-label", `Switch to ${nextTheme} mode`);
    themeColor.content = theme === "dark" ? "#101b1d" : "#f6f8f5";
  };
  themeToggle.addEventListener("click", () => {
    const theme = currentTheme() === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = theme;
    try {
      localStorage.setItem("theme", theme);
    } catch {}
    updateThemeToggle();
  });
  colorScheme.addEventListener("change", () => {
    if (!document.documentElement.dataset.theme) updateThemeToggle();
  });
  updateThemeToggle();
  if (window.AsciinemaPlayer) {
    const container = document.getElementById("player");
    container.replaceChildren();
    AsciinemaPlayer.create("demo.cast", container, { fit: "width", cols: 86, rows: 18, loop: 3, poster: 'npt:0:16'});
  }
});
