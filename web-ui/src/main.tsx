import React from "react";
import ReactDOM from "react-dom/client";
import "@cloudscape-design/global-styles/index.css";
import { applyMode, Mode } from "@cloudscape-design/global-styles";
import App from "./App";

const savedMode = localStorage.getItem("dora_theme") ?? "dark";
applyMode(savedMode === "light" ? Mode.Light : Mode.Dark);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
