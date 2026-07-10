import { useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Container from "@cloudscape-design/components/container";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import SpaceBetween from "@cloudscape-design/components/space-between";
import ContentLayout from "@cloudscape-design/components/content-layout";

export default function Settings() {
  const [token, setToken] = useState(localStorage.getItem("dora_api_token") ?? "");

  const save = () => {
    if (token) {
      localStorage.setItem("dora_api_token", token);
    } else {
      localStorage.removeItem("dora_api_token");
    }
  };

  return (
    <ContentLayout header={<Header variant="h1">Settings</Header>}>
      <Container header={<Header variant="h2">API Connection</Header>}>
        <SpaceBetween size="l">
          <FormField
            label="Bearer Token"
            description="If the Dora API requires authentication, enter the token here. It is stored in your browser's localStorage."
          >
            <Input
              type="password"
              value={token}
              onChange={({ detail }) => setToken(detail.value)}
              placeholder="Enter API token"
            />
          </FormField>
          <Box>
            <Button variant="primary" onClick={save}>
              Save
            </Button>
          </Box>
        </SpaceBetween>
      </Container>
    </ContentLayout>
  );
}
