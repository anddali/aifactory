import { useEffect, useState, type FormEvent } from 'react';
import { Navigate, useSearchParams } from 'react-router';
import { api, type AuthConfig } from '../api';
import { useAuth } from '../lib/auth';

function safeNext(next: string | null): string {
  // Only allow same-origin relative paths.
  if (!next || !next.startsWith('/') || next.startsWith('//')) return '/';
  return next;
}

export function SignInPage() {
  const { user, loading, refresh } = useAuth();
  const [params] = useSearchParams();
  const next = safeNext(params.get('next'));
  const [config, setConfig] = useState<AuthConfig | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const [email, setEmail] = useState('');
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .authConfig()
      .then(setConfig)
      .catch((e: Error) => setConfigError(e.message));
  }, []);

  if (!loading && user) return <Navigate to={next} replace />;

  const devLogin = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.devLogin(email.trim(), name.trim() || undefined);
      await refresh();
    } catch (err) {
      setError((err as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="signin">
      <div className="signin-card">
        <div className="signin-brand">
          <span className="brand-mark">◧</span> AI Factory
        </div>
        <p className="muted">Control plane for containerised coding agents.</p>
        {configError ? <div className="banner banner-error">Could not load sign-in options: {configError}</div> : null}
        {!config && !configError ? <div className="muted">Loading…</div> : null}
        {config?.entra ? (
          <a className="button primary block" href={api.entraLoginUrl}>
            Sign in with Microsoft
          </a>
        ) : null}
        {config?.entra && config?.dev_login ? <div className="divider">or</div> : null}
        {config?.dev_login ? (
          <form className="form" onSubmit={devLogin}>
            <div className="dev-note">Development sign-in</div>
            <label>
              Email
              <input type="email" required autoFocus value={email} onChange={(e) => setEmail(e.target.value)} placeholder="you@example.com" />
            </label>
            <label>
              Name <span className="muted">(optional)</span>
              <input value={name} onChange={(e) => setName(e.target.value)} />
            </label>
            {error ? <div className="banner banner-error">{error}</div> : null}
            <button className="primary block" type="submit" disabled={busy || !email.trim()}>
              {busy ? 'Signing in…' : 'Sign in'}
            </button>
          </form>
        ) : null}
        {config && !config.entra && !config.dev_login ? (
          <div className="banner banner-error">No sign-in method is configured on this plane.</div>
        ) : null}
      </div>
    </div>
  );
}
