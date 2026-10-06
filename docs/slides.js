window.addEventListener("load", () => {
  if (!window.Reveal) return;

  Reveal.initialize({
    controls: true,
    controlsTutorial: false,
    progress: true,
    slideNumber: "c/t",
    hash: true,
    keyboard: true,
    overview: true,
    center: false,
    transition: "fade",
    width: 1280,
    height: 720,
    margin: 0.1,
    minScale: 0.25,
    maxScale: 2,
  });
});
