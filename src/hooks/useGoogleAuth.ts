import { useState, useCallback, useEffect } from 'react'
import { open } from '@tauri-apps/plugin-shell'
import { start as startOAuth, onUrl } from '@fabianlars/tauri-plugin-oauth'

// PKCE flow: no client secret embedded (Google "Desktop app" clients use PKCE).
const CU_BACKEND = import.meta.env.VITE_CU_BACKEND_URL || 'http://localhost:8000'

function base64UrlEncode(bytes: Uint8Array): string {
  let s = ''
  for (const b of bytes) s += String.fromCharCode(b)
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')
}
function randomVerifier(): string {
  const bytes = new Uint8Array(48)
  crypto.getRandomValues(bytes)
  return base64UrlEncode(bytes)
}
async function challengeFromVerifier(verifier: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(verifier))
  return base64UrlEncode(new Uint8Array(digest))
}

interface AuthState {
  isAuthenticated: boolean
  accessToken: string | null
  userEmail: string | null
  userName: string | null
  isLoading: boolean
  error: string | null
}

export function useGoogleAuth() {
  const [state, setState] = useState<AuthState>({
    isAuthenticated: false,
    accessToken: null,
    userEmail: null,
    userName: null,
    isLoading: false,
    error: null
  })

  // Restore an existing backend session on mount. The backend session token
  // (set by login() after the /auth/google exchange) is the only credential we
  // trust here — no Google access token, no Rust round-trip.
  const checkAuth = useCallback(async () => {
    const token = localStorage.getItem('screen_buddy_session_token')
    const expires = localStorage.getItem('screen_buddy_session_expires')
    const email = localStorage.getItem('screen_buddy_email')

    const expiresMs = expires ? Date.parse(expires) : NaN
    const stillValid = !!token && Number.isFinite(expiresMs) && expiresMs > Date.now()

    if (stillValid) {
      setState(prev => ({
        ...prev,
        isAuthenticated: true,
        accessToken: token,
        userEmail: email || null,
        isLoading: false,
        error: null
      }))
    } else {
      // Missing or expired session — clear and stay logged out.
      localStorage.removeItem('screen_buddy_session_token')
      localStorage.removeItem('screen_buddy_session_expires')
      localStorage.removeItem('screen_buddy_email')
      setState(prev => ({ ...prev, isAuthenticated: false, accessToken: null }))
    }
  }, [])

  const login = useCallback(async () => {
    setState(prev => ({ ...prev, isLoading: true, error: null }))

    try {
      const clientId = '1012954378942-v08m777jd70vpsudrvohcvpv1orfif6e.apps.googleusercontent.com'
      const scopes = encodeURIComponent('openid email profile')

      // Start OAuth server - returns the port number
      const port = await startOAuth({
        ports: [8788, 8789, 8790],
        response: `
          <html>
            <head><title>Signed in — ScreenBuddy</title><meta http-equiv="refresh" content="0;url=https://screenbuddy.kapari.ai/signed-in"><script>window.location.replace('https://screenbuddy.kapari.ai/signed-in')</script></head>
            <body style="font-family: -apple-system, sans-serif; display: flex; justify-content: center; align-items: center; height: 100vh; margin: 0; background: #0a0a0a; color: #fff;">
              <div style="text-align: center;">
                <h1 style="color: #D4AF37;">Signed in &#10003;</h1>
                <p>Redirecting&hellip; you can return to ScreenBuddy.</p>
              </div>
            </body>
          </html>
        `
      })

      // Create promise that resolves when OAuth callback is received
      const callbackPromise = new Promise<string>((resolve) => {
        onUrl((url) => {
          resolve(url)
        })
      })

      // PKCE: a verifier for this login + its S256 challenge
      const codeVerifier = randomVerifier()
      const codeChallenge = await challengeFromVerifier(codeVerifier)

      // Build OAuth URL with actual port
      const redirectUri = `http://localhost:${port}/callback`
      const authUrl = `https://accounts.google.com/o/oauth2/v2/auth?client_id=${clientId}&redirect_uri=${encodeURIComponent(redirectUri)}&response_type=code&scope=${scopes}&access_type=offline&prompt=consent&code_challenge=${codeChallenge}&code_challenge_method=S256`

      // Open browser for OAuth
      await open(authUrl)

      // Wait for the callback with auth code
      const callbackUrl = await callbackPromise

      // Parse the auth code from callback URL
      const url = new URL(callbackUrl)
      const code = url.searchParams.get('code')
      const error = url.searchParams.get('error')

      if (error) {
        throw new Error(`OAuth error: ${error}`)
      }

      if (!code) {
        throw new Error('No authorization code received')
      }

      // Server-side PKCE exchange: send the code to OUR backend, which holds
      // the Google client secret, exchanges with Google, verifies the id_token,
      // and returns a backend session token. The secret never touches the client.
      const sresp = await fetch(`${CU_BACKEND}/auth/google`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ code, code_verifier: codeVerifier, redirect_uri: redirectUri }),
      })
      if (!sresp.ok) {
        throw new Error(`Login failed: ${sresp.status} ${await sresp.text()}`)
      }
      const sdata = await sresp.json()
      localStorage.setItem('screen_buddy_session_token', sdata.session_token)
      localStorage.setItem('screen_buddy_session_expires', sdata.expires_at)
      localStorage.setItem('screen_buddy_email', sdata.email || '')

      setState({
        isAuthenticated: true,
        accessToken: sdata.session_token,
        userEmail: sdata.email || null,
        userName: null,
        isLoading: false,
        error: null
      })

    } catch (err) {
      console.error('OAuth login failed:', err)
      setState(prev => ({
        ...prev,
        isLoading: false,
        error: err instanceof Error ? err.message : 'Login failed'
      }))
    }
  }, [])

  // Sliding refresh of the backend session. When the stored session is within
  // ~6h of expiry, exchange the current session token at /auth/refresh for a
  // fresh one. Contract: POST with `Authorization: Bearer <session_token>`,
  // response `{ session_token, expires_at }`. A 401 means the session is dead —
  // clear and force re-login.
  const REFRESH_WINDOW_MS = 6 * 60 * 60 * 1000 // 6 hours
  const refreshSessionIfNeeded = useCallback(async (): Promise<string | null> => {
    const token = localStorage.getItem('screen_buddy_session_token')
    const expires = localStorage.getItem('screen_buddy_session_expires')

    if (!token) return null

    const expiresMs = expires ? Date.parse(expires) : NaN
    // If we can't read an expiry, attempt a refresh to be safe; otherwise only
    // refresh once inside the sliding window.
    const needsRefresh =
      !Number.isFinite(expiresMs) || expiresMs - Date.now() < REFRESH_WINDOW_MS

    if (!needsRefresh) {
      return token // Session still comfortably valid.
    }

    try {
      const resp = await fetch(`${CU_BACKEND}/auth/refresh`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${token}` }
      })

      if (resp.status === 401) {
        // Session no longer accepted — clear and force re-login.
        localStorage.removeItem('screen_buddy_session_token')
        localStorage.removeItem('screen_buddy_session_expires')
        localStorage.removeItem('screen_buddy_email')
        setState({
          isAuthenticated: false,
          accessToken: null,
          userEmail: null,
          userName: null,
          isLoading: false,
          error: 'Session expired. Please sign in again.'
        })
        return null
      }

      if (!resp.ok) {
        console.error('Session refresh failed:', resp.status)
        return token // Keep current session; try again next interval.
      }

      const data = await resp.json()
      localStorage.setItem('screen_buddy_session_token', data.session_token)
      localStorage.setItem('screen_buddy_session_expires', data.expires_at)

      setState(prev => ({ ...prev, accessToken: data.session_token }))
      return data.session_token
    } catch (error) {
      console.error('Session refresh error:', error)
      return token // Network blip — keep the session and retry later.
    }
  }, [])

  const logout = useCallback(() => {
    localStorage.removeItem('screen_buddy_access_token')
    localStorage.removeItem('screen_buddy_expires_at')
    localStorage.removeItem('screen_buddy_email')
    localStorage.removeItem('screen_buddy_name')
    localStorage.removeItem('screen_buddy_refresh_token')
    localStorage.removeItem('screen_buddy_session_token')

    setState({
      isAuthenticated: false,
      accessToken: null,
      userEmail: null,
      userName: null,
      isLoading: false,
      error: null
    })
  }, [])

  // Periodic sliding refresh - check every 5 minutes while authenticated.
  useEffect(() => {
    if (!state.isAuthenticated) return

    const interval = setInterval(async () => {
      await refreshSessionIfNeeded()
    }, 5 * 60 * 1000) // 5 minutes

    return () => clearInterval(interval)
  }, [state.isAuthenticated, refreshSessionIfNeeded])

  return {
    ...state,
    login,
    logout,
    checkAuth,
    refreshSessionIfNeeded
  }
}
