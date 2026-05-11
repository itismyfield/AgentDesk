import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

interface Props {
  content: string;
  className?: string;
}

function isExternalHref(href?: string) {
  return Boolean(href && /^(https?:)?\/\//i.test(href));
}

export default function MarkdownContent({ content, className }: Props) {
  if (!content.trim()) return null;

  const classes = ["pcd-markdown", className].filter(Boolean).join(" ");

  return (
    <div className={classes}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ node, ...props }) => {
            const externalProps = isExternalHref(props.href)
              ? { target: "_blank", rel: "noopener noreferrer" }
              : {};

            return <a {...props} {...externalProps} />;
          }
        }}
      >
        {content.replace(/\r\n/g, "\n")}
      </ReactMarkdown>
    </div>
  );
}
