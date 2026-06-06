FROM golang:1.25-alpine AS build

WORKDIR /src
COPY go.mod ./
RUN go mod download

COPY . .
RUN go mod tidy && go build -o /out/control .

FROM alpine:3.20
WORKDIR /app
COPY --from=build /out/control /app/control

EXPOSE 8080
CMD ["/app/control"]