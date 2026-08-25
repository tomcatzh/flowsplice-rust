/* IosFrame follows the provided huashu-design iPhone 15 Pro frame geometry. */
function statusIcons(battery) {
  return `<span class="signal"><i></i><i></i><i></i><i></i></span>
    <svg class="wifi" viewBox="0 0 16 12" aria-hidden="true"><path d="M8 11.5a1 1 0 100-2 1 1 0 000 2z" fill="currentColor"/><path d="M3 7.5a7 7 0 0110 0" stroke="currentColor" stroke-width="1.3" fill="none" stroke-linecap="round"/><path d="M1 4.5a11 11 0 0114 0" stroke="currentColor" stroke-width="1.3" fill="none" stroke-linecap="round" opacity=".7"/></svg>
    <span class="battery"><i style="width:${battery}%"></i><b></b></span>`;
}

function IosFrame({ content, width = 393, height = 852, time = "9:41", battery = 100, darkMode = false, showDynamicIsland = true, tablet = false }) {
  return `<div class="ios-wrapper ${tablet ? "tablet" : "phone"} ${darkMode ? "dark-frame" : ""}">
    <div class="ios-screen" style="width:${width}px;height:${height}px">
      <div class="ios-status"><span>${time}</span><span class="status-icons">${statusIcons(battery)}</span></div>
      ${showDynamicIsland && !tablet ? '<div class="dynamic-island"></div>' : ''}
      <div class="ios-content">${content}</div>
      ${tablet ? '' : '<div class="home-indicator"></div>'}
    </div>
  </div>`;
}

document.querySelectorAll("[data-ios-frame]").forEach((host) => {
  const template = host.querySelector("template");
  const tablet = host.dataset.device === "ipad";
  host.innerHTML = IosFrame({
    content: template.innerHTML,
    width: tablet ? 744 : 393,
    height: tablet ? 1133 : 852,
    time: host.dataset.time || "9:41",
    battery: Number(host.dataset.battery || 92),
    darkMode: host.dataset.dark === "true",
    showDynamicIsland: !tablet,
    tablet,
  });
});
document.addEventListener("click", (event) => {
  const action = event.target.closest("[data-screen]");
  if (!action) return;
  const device = action.closest(".app-shell");
  if (!device) return;
  device.querySelectorAll("[data-screen]").forEach((item) => item.classList.toggle("selected", item.dataset.screen === action.dataset.screen));
  device.querySelectorAll("[data-panel]").forEach((panel) => panel.hidden = panel.dataset.panel !== action.dataset.screen);
});
