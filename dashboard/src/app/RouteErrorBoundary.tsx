import { Component, type ErrorInfo, type ReactNode } from "react";
import { AlertTriangle } from "lucide-react";
import { Desc, KitCard, Well } from "../components/common/KitPrimitives";
import { Button } from "../kit/free/button-clean";

interface RouteErrorBoundaryProps {
  resetKey: string;
  isKo: boolean;
  onGoHome: () => void;
  children: ReactNode;
}

interface RouteErrorBoundaryState {
  error: Error | null;
}

export class RouteErrorBoundary extends Component<RouteErrorBoundaryProps, RouteErrorBoundaryState> {
  state: RouteErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: unknown): RouteErrorBoundaryState {
    return { error: error instanceof Error ? error : new Error(String(error)) };
  }

  componentDidUpdate(prevProps: RouteErrorBoundaryProps) {
    if (this.state.error && prevProps.resetKey !== this.props.resetKey) {
      this.setState({ error: null });
    }
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("RouteErrorBoundary:", error, info);
  }

  private handleRetry = () => {
    this.setState({ error: null });
  };

  private handleGoHome = () => {
    this.setState({ error: null });
    this.props.onGoHome();
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    const { isKo } = this.props;
    const message = error.message.trim();

    return (
      <div
        role="alert"
        data-testid="route-error-boundary"
        className="flex h-full min-h-64 items-center justify-center p-4 sm:p-6"
      >
        <KitCard pad="md" className="w-full max-w-md">
          <div className="flex flex-col gap-3">
            <div className="flex items-start gap-2.5">
              <AlertTriangle size={18} aria-hidden className="mt-0.5 shrink-0 text-[var(--warn)]" />
              <div className="min-w-0 flex-1">
                <h2 className="m-0 text-base leading-6 font-semibold text-[var(--fg)]">
                  {isKo ? "화면을 표시하지 못했어요." : "Could not render this page."}
                </h2>
                <Desc className="mt-1 block">
                  {isKo
                    ? "라우트 렌더링 중 오류가 발생했어요. 다시 시도하거나 홈으로 이동해 주세요."
                    : "An error occurred while rendering this route. Try again or return to Home."}
                </Desc>
              </div>
            </div>
            {message ? (
              <Well className="px-3 py-2 font-mono text-xs leading-5 break-words text-[var(--fg-muted)]">
                {message}
              </Well>
            ) : null}
            <div className="flex flex-wrap items-center justify-end gap-2 pt-1">
              <Button variant="secondary" size="md" data-testid="route-error-home" onClick={this.handleGoHome}>
                {isKo ? "홈으로 이동" : "Go to Home"}
              </Button>
              <Button variant="primary" size="md" data-testid="route-error-retry" onClick={this.handleRetry}>
                {isKo ? "다시 시도" : "Try again"}
              </Button>
            </div>
          </div>
        </KitCard>
      </div>
    );
  }
}
