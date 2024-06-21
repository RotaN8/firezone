import Post from "@/components/Blog/Post";
import Content from "./readme.mdx";
import { Metadata } from "next";
import gravatar from "@/lib/gravatar";

export const metadata: Metadata = {
  title: "Secure remote access • Firezone",
  description: "Secure remote access makes remote work a win-win",
};

export default function Page() {
  return (
    <Post
      authorName="Jeff Spencer"
      authorTitle=""
      authorAvatarSrc={gravatar("jeff@firezone.dev")}
      title="Secure remote access makes remote work a win-win"
      date="November 17, 2023"
    >
      <Content />
    </Post>
  );
}
