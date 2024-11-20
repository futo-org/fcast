const path = require('path');

module.exports = [
    {
        mode: 'development',
        entry: './src/App.ts',
        target: 'electron-main',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: /src/,
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: 'bundle.js',
            path: path.resolve(__dirname, 'dist'),
        },
    },
    {
        mode: 'development',
        entry: {
            preload: './src/main/Preload.ts',
            renderer: './src/main/Renderer.ts',
        },
        target: 'electron-renderer',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: /src/,
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            path: path.resolve(__dirname, 'dist/main'),
        },
    },
    {
        mode: 'development',
        entry: {
            preload: './src/player/Preload.ts',
            renderer: './src/player/Renderer.ts',
        },
        target: 'electron-renderer',
        module: {
            rules: [
                {
                    test: /\.tsx?$/,
                    include: /src/,
                    use: [{ loader: 'ts-loader' }]
                }
            ],
        },
        resolve: {
            extensions: ['.tsx', '.ts', '.js'],
        },
        output: {
            filename: '[name].js',
            path: path.resolve(__dirname, 'dist/player'),
        },
    }
];