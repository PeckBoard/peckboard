import { Component, type ErrorInfo, type ReactNode } from 'react'

interface ErrorBoundaryProps {
  children: ReactNode
  /** Optional label shown in the fallback so the user knows which part
   *  of the UI failed (e.g. "chat view"). Defaults to "view". */
  label?: string
  /** When this value changes, a tripped boundary resets and re-renders
   *  its children — e.g. pass the active session id so navigating to a
   *  different session escapes a crashed view. */
  resetKey?: unknown
}

interface ErrorBoundaryState {
  error: Error | null
}

export default class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error('ErrorBoundary caught render error:', error, info.componentStack)
  }

  componentDidUpdate(prevProps: ErrorBoundaryProps) {
    if (this.state.error && prevProps.resetKey !== this.props.resetKey) {
      this.setState({ error: null })
    }
  }

  render() {
    if (this.state.error) {
      return (
        <div className="error-boundary" role="alert" data-testid="error-boundary">
          <p className="error-boundary-title">
            Something went wrong in this {this.props.label ?? 'view'}.
          </p>
          <p className="error-boundary-detail">{this.state.error.message}</p>
          <button
            type="button"
            className="error-boundary-retry"
            onClick={() => this.setState({ error: null })}
          >
            Try again
          </button>
        </div>
      )
    }
    return this.props.children
  }
}
