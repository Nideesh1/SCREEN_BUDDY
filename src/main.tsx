import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import './index.css'

// App is the auth gate: it shows the splash login until a backend session is
// present, then renders the authenticated shell around AgentRunPanel.
ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
